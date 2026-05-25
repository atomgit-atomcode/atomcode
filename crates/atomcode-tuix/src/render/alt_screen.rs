// crates/atomcode-tuix/src/render/alt_screen.rs
//
// Alt-screen renderer (Phase 1: skeleton).
//
// AltScreenRenderer takes over the terminal's alternate screen buffer
// (`\x1b[?1049h`) and paints into it with absolute cursor positioning,
// bypassing DECSTBM scroll regions entirely. This is the strategy
// vim / htop / less / Claude Code / opencode all use, and is the
// answer for terminals (JetBrains JediTerm, legacy Windows conhost)
// that don't fully implement DECSTBM but DO support alt-screen.
//
// Trade-off: the host terminal's native scrollback is unavailable
// while the app is running — Cmd+Up / Page Up in the host terminal
// won't reach above the alt-screen. The app provides its own internal
// scrollback navigation (Phase 2) instead. On exit, the alt-screen is
// popped and the host terminal returns to its pre-app state.
//
// See `docs/superpowers/specs/2026-04-29-alt-screen-renderer-design.md`
// for the full design and phasing.
//
// PHASE 1 SCOPE (this file): skeleton only.
//   * Renderer trait stubbed — most arms are no-op
//   * Welcome banner rendered at fixed rows (no body buffer yet)
//   * Alt-screen enter on construct, pop on Drop
//   * Routes from `lib.rs` only via `ATOMCODE_ALT=1` user opt-in
//
// Later phases bring in the body_lines buffer, scrollback navigation,
// pinned input box / status bar / spinner, resize handling, and
// auto-detection. See spec §Phasing.

use std::io::{self, BufWriter, Stdout, Write};

use super::{MenuPayload, Renderer, StatusLine, UiLine};
use super::selection::{
    line_display_width_sgr_aware,
    render_line_with_selection, selection_col_range_for_line,
    truncate_to_width_sgr_aware,
};
#[cfg(test)]
use super::selection::extract_line_selection_text;
use crate::i18n::{t, Msg};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;
use crate::width::{cluster_width, display_width, truncate_to_width};
use unicode_segmentation::UnicodeSegmentation;


/// Soft-wrap `s` into chunks each ≤ `max_cols` display columns, using
/// the same CSI-aware parser as `truncate_to_width_sgr_aware`. Used by
/// `push_command_output` so long single-line content (notably the OAuth
/// URL printed during `/login`) survives `paint_body`'s width-truncation
/// step instead of being clipped at the right edge — clipped lines can't
/// be selected for copy in alt-screen mode.
///
/// Wraps at character boundaries (no word-break logic): URLs are the
/// motivating case, and they have no whitespace anyway. SGR spans that
/// straddle a wrap point are not re-emitted on the next chunk; for the
/// uncoloured content this fn is currently fed (URLs, plain log lines)
/// that's a non-issue, and `paint_body` writes a trailing `\x1b[0m` per
/// row so dangling spans don't bleed into adjacent rows.
///
/// Empty input returns `vec![String::new()]` so callers preserve blank
/// lines (the previous `for line in safe.split('\n')` invariant).
fn wrap_to_width_sgr_aware(s: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return vec![String::new()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut acc = String::new();
    let mut cols = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if bytes[i] == 0x1b && i + 1 < s.len() && bytes[i + 1] == b'[' {
            let start = i;
            i += 2;
            while i < s.len() {
                let c = bytes[i];
                i += 1;
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            acc.push_str(&s[start..i]);
            continue;
        }
        let next = s[i..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(idx, _)| idx + i)
            .unwrap_or(s.len());
        let g = &s[i..next];
        let w = cluster_width(g);
        if w > 0 && cols + w > max_cols {
            chunks.push(std::mem::take(&mut acc));
            cols = 0;
        }
        acc.push_str(g);
        cols += w;
        i = next;
    }
    chunks.push(acc);
    chunks
}


// SGR sequences used inline in body strings. Same set PlainRenderer
// already uses; keeping them duplicated rather than re-exported because
// alt_screen will diverge from plain on more dimensions in later phases
// and shared constants would create a noisy upstream-change footprint.
const SGR_RESET: &str = "\x1b[0m";
const SGR_RED: &str = "\x1b[91m";
const SGR_GREEN: &str = "\x1b[92m";
const SGR_MAGENTA: &str = "\x1b[95m"; // Role::Brand — see render/theme.rs
const SGR_CYAN: &str = "\x1b[96m"; // Role::Border / Accent — bright variant; the
                                   // dim 36m form rendered the input-box rule
                                   // as visibly "dashed" on Windows Terminal
                                   // because the muted cyan let font-glyph
                                   // gaps in `─` show through. Bright cyan
                                   // matches retained's `Palette::BORDER`
                                   // (Color::Cyan ≡ SGR 96 in crossterm) and
                                   // closes the cross-renderer drift.
const SGR_DIM: &str = "\x1b[2m";
const SGR_GREY: &str = "\x1b[90m"; // Role::Muted — bright black / mid-gray.
                                   // Prefer over SGR 2m on Windows conhost
                                   // (< 1809 historically swallowed dim);
                                   // matches retained's `Palette::MUTED`
                                   // which crossterm emits as SGR 90.
const SGR_BOLD: &str = "\x1b[1m";
const SGR_YELLOW: &str = "\x1b[93m";
/// Reverse video — swap fg/bg. Combined with a coloured fg this paints
/// a coloured "chip" with the underlying default-bg as the chip's text
/// colour. Used by the approval-prompt Y/A/N badges.
const SGR_REVERSE: &str = "\x1b[7m";

/// Default cap on `body_lines` length. ~5000 rows × ~200 bytes/row
/// (rough average for SGR-decorated text) is ~1 MB per session — fine
/// for our tier. Override via `ATOMCODE_SCROLLBACK_ROWS`.
const DEFAULT_SCROLLBACK_ROWS: usize = 5000;

fn scrollback_rows_from_env() -> usize {
    std::env::var("ATOMCODE_SCROLLBACK_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 100)
        .unwrap_or(DEFAULT_SCROLLBACK_ROWS)
}

/// Alt-screen anchored renderer. See module-level doc.
pub struct AltScreenRenderer<W: Write + Send> {
    out: W,
    caps: TerminalCaps,
    /// True iff we successfully entered the alt-screen on construction.
    /// Drop pops only when this is true so a failed enter doesn't try
    /// to pop a buffer we never owned.
    alt_screen_active: bool,
    /// Saved Win32 console-input mode captured when we flipped the
    /// mouse-capture bits. `Drop` / `leave_alt_screen` write this back
    /// so the parent shell gets its quick-edit / line-input state
    /// returned exactly as it was, not approximated. `None` means we
    /// never successfully read the original (e.g. stdin not a console)
    /// — in that case we don't try to restore. Windows-only because
    /// other platforms route mouse capture through VT escape codes.
    #[cfg(windows)]
    prior_console_in_mode: Option<u32>,
    /// Cached width / height. Updated by resize in Phase 4.
    width: u16,
    height: u16,
    /// All body rows ever pushed, oldest-first. Each row is a single
    /// physical line of text (with embedded SGR colour escapes).
    /// `paint_body` paints a slice of this against the current viewport;
    /// no terminal-side scrollback is involved (alt-screen owns the
    /// whole viewport, host terminal's scrollback is unreachable).
    body_lines: Vec<String>,
    /// Message boundary markers for "jump to prev/next message" navigation.
    /// Tracks which line_idx marks the start of a User / Assistant / ToolCall / ToolResult message.
    message_marks: Vec<crate::render::MessageMark>,
    /// True if the last mark pushed was `MarkKind::Assistant`. Used to de-duplicate
    /// marks for multi-chunk `UiLine::AssistantText` streams — only the first chunk
    /// of a turn gets a new mark; subsequent chunks within the same assistant turn are silent.
    /// Cleared whenever a User / ToolCall / ToolCallInFlight / TurnSeparator fires.
    last_mark_was_assistant: bool,
    /// Raw (unwrapped) body rows — mirrors `body_lines` but stores each
    /// logical line *before* soft-wrapping. Used by `reflow_body_lines`
    /// on resize so that widening the terminal re-merges previously
    /// split short rows back into their original long form. Each entry
    /// corresponds 1:1 with one call to `push_body_row_raw`; rows that
    /// were already short enough to not need wrapping appear identically
    /// in both `raw_body_lines` and `body_lines`.
    raw_body_lines: Vec<String>,
    /// Index into `body_lines` for the FIRST visible body row. Auto-
    /// tracks the tail when `sticky_bottom` is true (most common case);
    /// only diverges from "tail" when the user is actively scrolled up
    /// via PageUp / Home / scroll_body.
    viewport_top: usize,
    /// True iff the user is at the bottom of body_lines. New content
    /// auto-scrolls when true; held position when false. Toggled by
    /// scroll_body / scroll_body_to_top / scroll_body_to_bottom.
    sticky_bottom: bool,
    /// Bound on body_lines length. Front rows drop when exceeded so
    /// memory stays flat for very long sessions.
    max_scrollback_rows: usize,
    /// Line-buffer for streaming assistant text. Chunks accumulate
    /// here until `\n` or `AssistantLineBreak`; the completed line
    /// is then run through the markdown renderer and pushed to
    /// `body_lines` as one entry.
    assistant_line_buf: String,
    /// Markdown parser state (code-block tracking, table buffering)
    /// shared across consecutive assistant lines so a fenced code
    /// block opened on one chunk stays open on the next. Reset on
    /// every new `UiLine::User` (new turn) so a previous turn's
    /// stuck-open fence doesn't bleed into the user's prompt.
    md_state: crate::markdown::MdState,
    /// True when widget state has changed since the last body paint.
    /// Set on every `push_body_row`; cleared by `paint_body`. Reduces
    /// redundant repaints when one render() call pushes multiple rows
    /// (e.g. TurnSeparator's three rows or DiffBlock's many).
    body_dirty: bool,
    // ── Phase 3+: footer ──
    /// Most-recent input prompt state — `(buf, cursor_byte)`. Kept so
    /// `paint_footer` can re-render the input row even when triggered
    /// by a non-InputPrompt event (e.g. a body push during streaming
    /// would otherwise leave a stale input row from before).
    pending_input: Option<(String, usize)>,
    /// Most-recent status line. Pulled from `UiLine::InputPrompt` /
    /// `UiLine::StreamingBox`. Default-initialised so paint_footer can
    /// always render *something* (empty string) before the first
    /// InputPrompt arrives.
    pending_status: StatusLine,
    /// Active spinner state — `(frame, label)`. `Some` during streaming
    /// (paint shows it ABOVE the input row); `None` resumes the plain
    /// input prompt. Toggled by `Spinner` / `StreamingBox` /
    /// `ClearTransient`.
    pending_spinner: Option<(&'static str, String)>,
    /// Slash-command palette items + selected index. Carried through
    /// from `UiLine::InputPrompt` / `UiLine::StreamingBox`'s `menu`
    /// field. None → no menu paint. Up to 4 items shown at once;
    /// pagination around `selected` when there are more.
    pending_menu: Option<MenuPayload>,
    /// Image-attachment marker numbers currently visible inside the
    /// input buffer (intersection of typed `[Image #N]` literals with
    /// real pending bytes — see `event_loop::compute_input_attachments`).
    /// Each gets a `└ [Image #N]` preview row rendered between the
    /// bot_rule and the menu, mirroring the retained renderer.
    pending_attachments: Vec<usize>,
    /// True when footer state changed since the last paint. Same role
    /// as `body_dirty` but for the footer strip.
    footer_dirty: bool,
    /// Shared selection state — tracks the active drag anchor/head and
    /// whether a drag is in progress. Delegates to
    /// `crate::render::selection::SelectionState` so the selection
    /// logic is shared with the retained renderer. Cleared on
    /// `reset` / `clear_screen` / `on_resize` since each can
    /// invalidate either the line indices (reset) or the display
    /// columns (resize → re-flow at paint time).
    selection: crate::render::selection::SelectionState,
    /// Tracks whether the terminal cursor is currently shown (`?25h`
    /// last emitted) or hidden (`?25l`). Used to dedupe visibility
    /// toggles per frame: re-emitting `?25h` at streaming framerate
    /// restarts the host terminal's hardware cursor blink animation,
    /// which on macOS Terminal.app reads as constant flicker even
    /// after `?12l` disabled hardware blink (the show pulse itself is
    /// the visible flash). Initialised to `true` because terminals
    /// default to a visible cursor.
    cursor_shown: bool,
    /// True on terminals that process CUP sequences synchronously
    /// (JediTerm, legacy conhost) — paint_body's per-row CUPs would
    /// otherwise visibly trail the cursor through every body row.
    /// On those we hide cursor before paint_body and re-show in
    /// paint_footer's tail. False on fast terminals (macOS Terminal.app,
    /// iTerm2, modern xterm, WezTerm, Kitty), where paint completes in
    /// well under a frame and the per-frame hide/show toggle reads
    /// instead as flicker — we leave cursor visible the whole time
    /// and only reposition it via a CUP at frame end.
    slow_paint_terminal: bool,
    /// Whether to paint the right-side vertical scrollbar. Persisted to
    /// ui-state.toml so the setting survives restarts. Toggled via
    /// `toggle_scrollbar()`. P6.4 uses this field in paint_body.
    pub show_scrollbar: bool,
}

impl AltScreenRenderer<BufWriter<Stdout>> {
    pub fn new(caps: TerminalCaps, slow_paint_terminal: bool) -> Self {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        let mut r = Self::with_writer(BufWriter::new(io::stdout()), caps, w, h);
        r.slow_paint_terminal = slow_paint_terminal;
        r
    }
}

impl<W: Write + Send> AltScreenRenderer<W> {
    pub fn with_writer(out: W, caps: TerminalCaps, w: u16, h: u16) -> Self {
        let show_scrollbar = crate::render::ui_state::load().ui.show_scrollbar;
        let mut r = Self {
            out,
            caps,
            alt_screen_active: false,
            #[cfg(windows)]
            prior_console_in_mode: None,
            width: w,
            height: h,
            body_lines: Vec::new(),
            message_marks: Vec::new(),
            last_mark_was_assistant: false,
            raw_body_lines: Vec::new(),
            viewport_top: 0,
            sticky_bottom: true,
            max_scrollback_rows: scrollback_rows_from_env(),
            assistant_line_buf: String::new(),
            md_state: crate::markdown::MdState::new(),
            body_dirty: false,
            pending_input: None,
            pending_status: StatusLine::default(),
            pending_spinner: None,
            pending_menu: None,
            pending_attachments: Vec::new(),
            footer_dirty: true,
            selection: crate::render::selection::SelectionState::default(),
            cursor_shown: true,
            slow_paint_terminal: false,
            show_scrollbar,
        };
        r.enter_alt_screen();
        r
    }

    /// Number of menu rows to paint. Capped at 4 (matches retained's
    /// pagination) so a 50-command match list doesn't squeeze body
    /// content off the screen.
    fn menu_paint_rows(&self) -> u16 {
        self.pending_menu
            .as_ref()
            .map(|m| m.items.len().min(4) as u16)
            .unwrap_or(0)
    }

    /// Total rows reserved for the footer. Variable because the
    /// slash-menu palette + attachment preview rows grow / shrink the
    /// footer dynamically:
    ///   spinner (1) + top_rule (1) + input (1) + bot_rule (1)
    ///   + attachments (0..N) + menu (0..4) + status (1) = 5..N+9
    fn footer_rows(&self) -> u16 {
        // spinner + top_rule + input + bot_rule + status = 5 base
        5 + self.menu_paint_rows() + self.pending_attachments.len() as u16
    }

    /// Body region height = total rows − footer rows. Always at least 1
    /// so `paint_body` never tries to write to row 0 / row N+ on tiny
    /// terminals. When the terminal is so short the footer wouldn't fit,
    /// we degrade to body_height=1 and the footer overflows the bottom —
    /// visually broken but not crashing.
    fn body_height(&self) -> u16 {
        self.height.saturating_sub(self.footer_rows()).max(1)
    }

    /// Switch to alt-screen, home cursor, clear it, enable mouse
    /// capture. Sequences:
    ///   * `\x1b[?1049h` — save main screen + switch to alt
    ///   * `\x1b[H\x1b[2J` — home cursor + clear screen
    ///   * `\x1b[?1002h` — button-event tracking: report button
    ///     presses, releases, AND motion-while-button-held. This is
    ///     a strict superset of `?1000h` (which only reports presses)
    ///     and is what we need so drag-selection sees per-cell motion
    ///     instead of just the down + up endpoints. Scroll-wheel
    ///     events (buttons 4/5) ride the same channel and are
    ///     unaffected by the upgrade.
    ///   * `\x1b[?1006h` — SGR-extended coordinates (replaces the
    ///     legacy fixed-byte format that breaks past col 223)
    ///   * `\x1b[?12l` — disable cursor blinking. macOS Terminal.app's
    ///     hardware blink restarts on every show-cursor (`\x1b[?25h`),
    ///     so paint_frame's hide→repaint→show cycle (one per keystroke)
    ///     looked like a non-stop flicker. Restored to `?12h` on leave.
    ///
    /// Best-effort: if the writer fails, `alt_screen_active` stays
    /// false and Drop won't try to pop.
    fn enter_alt_screen(&mut self) {
        let seq = "\x1b[?1049h\x1b[H\x1b[2J\x1b[?1002h\x1b[?1006h\x1b[?12l";
        if self.out.write_all(seq.as_bytes()).is_ok() && self.out.flush().is_ok() {
            self.alt_screen_active = true;
            // Legacy Windows conhost (Win10 PowerShell 5/7, cmd.exe)
            // does NOT implement the VT mouse-mode toggles above —
            // `?1002h` / `?1006h` parse as no-ops. Mouse events only
            // flow when `ENABLE_MOUSE_INPUT` is set on the console
            // input handle via `SetConsoleMode`, AND when
            // `ENABLE_QUICK_EDIT_MODE` is cleared (otherwise conhost
            // intercepts mouse for text-selection and never delivers
            // events to the program — wheel ticks included on some
            // versions).
            //
            // We previously routed this through crossterm's
            // `EnableMouseCapture`. Field reports (Win10 PS7) showed
            // wheel still didn't work even after that fix shipped.
            // crossterm's Windows path calls `set_mode(ENABLE_MOUSE_
            // INPUT | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT)` —
            // an OVERWRITE of the entire mode. That:
            //   1. drops `ENABLE_VIRTUAL_TERMINAL_INPUT` and any
            //      other bits raw_mode set up,
            //   2. doesn't surface SetConsoleMode failures (we
            //      `let _ =` the result),
            //   3. relies on `ENABLE_QUICK_EDIT_MODE` being clearable
            //      via implicit-absent semantics in the new mask,
            //      which works on most conhost builds but isn't the
            //      shape Microsoft's own samples use.
            //
            // Switch to read-modify-write through windows-sys: read
            // the current mode, OR in the mouse bits, AND-out
            // `ENABLE_QUICK_EDIT_MODE` explicitly, write back. Save
            // the original for `leave_alt_screen` so the parent shell
            // gets its mode restored exactly. Surface the
            // GetConsoleMode/SetConsoleMode return codes via the
            // trace log so a "still doesn't work" report tells us
            // immediately whether the syscalls even succeeded.
            #[cfg(windows)]
            {
                self.prior_console_in_mode = crate::render::conhost::enable_conhost_mouse_capture();
            }
        }
    }

    /// Pop the alt-screen + disable mouse capture, restoring whatever
    /// was on the main screen before we entered. Called from
    /// `shutdown()` on normal exit and from `Drop` as belt-and-
    /// suspenders for panic paths. Sequences mirror the reverse of
    /// the enter set.
    fn leave_alt_screen(&mut self) {
        if self.alt_screen_active {
            // Disable mouse capture FIRST — if alt-screen pops while
            // mouse mode is still on, some terminals leak `\x1b[<...M`
            // events into the main screen until something resets them.
            // On Windows we additionally restore the exact pre-enter
            // SetConsoleMode bitmask we saved in `enter_alt_screen`,
            // so the parent shell gets its quick-edit / line-input
            // flags back as they were (not "approximated" by
            // crossterm's saved-original snapshot).
            #[cfg(windows)]
            {
                if let Some(prior) = self.prior_console_in_mode.take() {
                    crate::render::conhost::restore_conhost_console_in_mode(prior);
                }
            }
            let _ = self.out.write_all(b"\x1b[?25h\x1b[?12h\x1b[?1006l\x1b[?1002l\x1b[?1049l");
            let _ = self.out.flush();
            self.alt_screen_active = false;
        }
    }

    /// Append one row to body_lines, drop oldest if we'd exceed the
    /// scrollback cap, mark body dirty for the next paint. The single
    /// entry point so cap enforcement and dirty tracking can't be
    /// forgotten by individual UiLine arms.
    fn push_body_row(&mut self, row: String) {
        self.body_lines.push(row);
        // Bound the buffer. Drop from the front so the most-recent
        // content is preserved (the typical case is the user scrolled
        // to bottom; oldest content is least relevant).
        while self.body_lines.len() > self.max_scrollback_rows {
            self.body_lines.remove(0);
            self.message_marks.retain(|m| m.line_idx > 0);
            for m in self.message_marks.iter_mut() {
                m.line_idx -= 1;
            }
        }
        self.body_dirty = true;
    }

    /// Effective body width: when `show_scrollbar` is on, paint reserves
    /// the rightmost column for the scrollbar glyph (`│`/`█`), so body
    /// content must wrap at `width - 1`. Otherwise body uses the full
    /// `width`.
    ///
    /// We reserve the column based on `show_scrollbar` alone (not on
    /// runtime overflow) so reflow and paint always agree: reflowing
    /// at a wider width than paint uses would cause `truncate_to_width_
    /// sgr_aware` to silently chop the trailing cluster of any line
    /// that wrapped to exactly `width` cols. Slight cost: one col is
    /// reserved even when the buffer doesn't overflow yet — accepted
    /// in trade for predictable, non-chicken-and-egg sizing.
    fn effective_body_width(&self) -> usize {
        let raw = self.width as usize;
        if self.show_scrollbar {
            raw.saturating_sub(1).max(1)
        } else {
            raw
        }
    }

    /// Push a **raw** (not yet soft-wrapped) body row. The raw line is
    /// stored in `raw_body_lines` for later re-flow on resize, while
    /// the soft-wrapped chunks are pushed to `body_lines` for immediate
    /// rendering. Callers that produce logical lines longer than the
    /// terminal width should use this instead of `push_body_row` so
    /// `on_resize` can re-wrap at the new width without losing content.
    fn push_body_row_raw(&mut self, raw_line: String) {
        // Store raw line for re-flow on resize.
        self.raw_body_lines.push(raw_line.clone());
        // Soft-wrap at the effective body width (honors scrollbar
        // reservation) so reflow + paint use the same column budget.
        let max_w = self.effective_body_width();
        for chunk in wrap_to_width_sgr_aware(&raw_line, max_w) {
            self.push_body_row(chunk);
        }
    }

    /// Record the start of a new logical message in `message_marks`.
    /// The mark's `line_idx` is set to the CURRENT length of `body_lines`
    /// (i.e. the index the NEXT `push_body_row` will occupy).
    /// Called before any push in the render arm that starts a new message.
    fn mark_message(&mut self, kind: crate::render::MarkKind) {
        // Record both the body and raw indices. reflow_body_lines uses
        // raw_idx to re-derive line_idx after wrapping at a new width
        // — without it, marks would point at pre-reflow body positions
        // and Alt+↑/↓ would land on the wrong rows after `on_resize` or
        // `/scrollbar` toggle.
        self.message_marks.push(crate::render::MessageMark {
            line_idx: self.body_lines.len(),
            raw_idx: self.raw_body_lines.len(),
            kind,
        });
    }

    /// Re-flow `body_lines` from `raw_body_lines` at the current
    /// terminal width. Called by `on_resize` so that widening the
    /// terminal re-merges previously split short rows back into fewer
    /// longer rows, and narrowing splits long rows instead of
    /// truncating them (issue #363).
    fn reflow_body_lines(&mut self) {
        self.body_lines.clear();
        // Reflow at the effective body width — same column budget paint
        // will use. Reflowing at full `self.width` while paint truncates
        // to `width - 1` (scrollbar on) chops the trailing cluster of
        // every wrapped-to-exactly-full line.
        let max_w = self.effective_body_width();
        // Build a parallel `raw_to_body` map so we can remap each
        // message_mark.raw_idx to its new line_idx after wrapping.
        // Index i = "after consuming raw line i, body_lines.len() = ?".
        let mut raw_to_body: Vec<usize> = Vec::with_capacity(self.raw_body_lines.len() + 1);
        raw_to_body.push(0);
        for raw in &self.raw_body_lines {
            for chunk in wrap_to_width_sgr_aware(raw, max_w) {
                self.body_lines.push(chunk);
            }
            raw_to_body.push(self.body_lines.len());
        }
        // Remap marks: a mark with raw_idx=k pointed at the first body
        // row produced by raw line k, which is now `raw_to_body[k]`.
        // raw_idx past the end (sentinel — never happened in practice
        // because marks fire before push) clamps to body_lines.len().
        for m in self.message_marks.iter_mut() {
            m.line_idx = raw_to_body
                .get(m.raw_idx)
                .copied()
                .unwrap_or(self.body_lines.len());
        }
        // Bound the buffer (same cap as push_body_row).
        while self.body_lines.len() > self.max_scrollback_rows {
            self.body_lines.remove(0);
            self.message_marks.retain(|m| m.line_idx > 0);
            for m in self.message_marks.iter_mut() {
                m.line_idx -= 1;
            }
        }
        // Also bound raw_body_lines to the same cap so the two buffers
        // don't drift over time. Sync `raw_idx` on each drain — marks
        // whose raw_idx hits 0 lose their anchor and get dropped.
        while self.raw_body_lines.len() > self.max_scrollback_rows {
            self.raw_body_lines.remove(0);
            self.message_marks.retain(|m| m.raw_idx > 0);
            for m in self.message_marks.iter_mut() {
                m.raw_idx -= 1;
            }
        }
    }

    /// Render the current state of body_lines into the viewport area.
    /// Phase 2 paints all visible rows on every dirty frame (no
    /// cell-diff against previous frame yet — full repaint per render
    /// call is fine at our event cadence). Cell-diff is a Phase 5+
    /// optimization for terminals where ANSI throughput matters.
    ///
    /// Visible window: `body_lines[viewport_start .. viewport_start + body_height]`
    /// where `viewport_start` honours `sticky_bottom` (auto-tail) when
    /// set, otherwise pins to `viewport_top` (Phase 3 keyboard
    /// handlers).
    ///
    /// Empty rows below the body content (when body_lines is shorter
    /// than the viewport, early in a session) are explicitly cleared
    /// so a previous frame's content can't ghost.
    fn paint_body(&mut self) {
        if !self.body_dirty {
            return;
        }
        // Phase 3: footer reserves bottom rows. body_height shrinks
        // accordingly so the input box / status bar never get
        // overwritten by body content.
        let body_height = self.body_height() as usize;
        let total = self.body_lines.len();

        // sticky_bottom: viewport_start is "last body_height rows"; if
        // body_lines is shorter than viewport, just start at 0 and
        // leave the bottom blank.
        let viewport_start = if self.sticky_bottom {
            total.saturating_sub(body_height)
        } else {
            self.viewport_top.min(total.saturating_sub(body_height))
        };

        // Compute scrollbar shape before the body loop so we know
        // whether to reserve the rightmost column for the thumb/track.
        // scrollbar::compute returns None when disabled or no overflow.
        let scrollbar_shape = crate::render::scrollbar::compute(
            total,
            body_height,
            viewport_start,
            self.sticky_bottom,
            self.show_scrollbar,
        );

        // Walk every row in the visible window. CUP each row, EL to
        // wipe leftover glyphs from previous frames, then write the
        // body content (trimmed to terminal width and SGR-terminated
        // so long lines don't autowrap into the next body row's slot
        // and stale colour spans don't bleed). For rows past the end
        // of body_lines, just EL (clear). 1-indexed rows.
        //
        // Use `effective_body_width()` so reflow and paint agree on
        // the column budget. Previously paint subtracted 1 only when
        // `scrollbar_shape.is_some()` (overflow active), but reflow
        // unconditionally used `self.width` — when scrollbar was on +
        // overflow happened, the trailing cluster of any
        // wrapped-to-exact-width line was silently chopped by
        // `truncate_to_width_sgr_aware`.
        let max_cols = self.effective_body_width();
        // Snapshot the ordered selection bounds once so the per-row
        // loop doesn't re-borrow `self.selection` while we hold a
        // reference to `self.body_lines[i]`. Cheap (Copy) and only
        // computed when a selection exists. `SelectionState.selection`
        // is `Option<Selection>` with `BodyPos = (usize, u16)`; convert
        // cols to usize for `selection_col_range_for_line`.
        let sel_bounds = self.selection.selection.map(|s| {
            let (lo, hi) = if s.anchor <= s.head { (s.anchor, s.head) } else { (s.head, s.anchor) };
            ((lo.0, lo.1 as usize), (hi.0, hi.1 as usize))
        });
        for row_idx in 0..body_height {
            let abs_row = (row_idx + 1) as u16;
            let cup_el = format!("\x1b[{};1H\x1b[K", abs_row);
            let _ = self.out.write_all(cup_el.as_bytes());
            let body_idx = viewport_start + row_idx;
            if body_idx < total {
                let line = &self.body_lines[body_idx];
                // SGR-aware: CSI escape sequences (`\x1b[...m`) take
                // zero visible columns and are passed through verbatim.
                // Without this, the `[`, digits, and final `m` of each
                // SGR pair eat into the visible-content budget — a
                // 80-col line with one colour span would lose 5+
                // trailing visible chars.
                let painted = match sel_bounds.and_then(|(lo, hi)| {
                    selection_col_range_for_line(body_idx, lo, hi, line)
                }) {
                    Some((s, e)) => render_line_with_selection(line, max_cols, s, e),
                    None => truncate_to_width_sgr_aware(line, max_cols),
                };
                let _ = self.out.write_all(painted.as_bytes());
                // Trailing SGR reset: in case the row had an open SGR
                // span at the truncation point (e.g. `\x1b[31mlong red
                // text...` cut mid-span), reset so the next row's
                // CUP+EL doesn't paint over already-coloured cells.
                // Cheap belt-and-suspenders — 4 bytes per row.
                let _ = self.out.write_all(b"\x1b[0m");
            }
        }

        // Paint right-side scrollbar. Executed AFTER the body rows so
        // the scrollbar glyphs overwrite whatever the body row emitted
        // in that column (for lines that were not truncated by the
        // max_cols guard above, e.g. very short lines).
        // scrollbar_col is 1-indexed = self.width (the rightmost column).
        if let Some(shape) = &scrollbar_shape {
            let scrollbar_col = self.width;
            for row_idx in 0..body_height {
                let target_row = 1 + row_idx as u16;
                let glyph = if crate::render::scrollbar::is_thumb_row(shape, row_idx) {
                    "\u{2588}" // █ FULL BLOCK
                } else {
                    "\u{2502}" // │ LIGHT VERTICAL
                };
                let seq = format!("\x1b[{};{}H{}", target_row, scrollbar_col, glyph);
                let _ = self.out.write_all(seq.as_bytes());
            }
        }

        // No flush here: paint_frame batches body + footer +
        // anchor_cursor_to_input into a single flush at the very
        // end so the terminal renders only the final cursor
        // position. Flushing mid-frame (after the per-row CUPs
        // walked the cursor through every body row) gave macOS
        // Terminal.app a vsync window to draw the cursor at
        // intermediate body positions before anchor moved it
        // back, which read as a cursor "blinking" mid-screen
        // during streaming. Tests call `r.flush()` explicitly
        // after `r.paint_body()`.
        self.body_dirty = false;
    }

    /// Map a screen-cell `(col, row)` (0-indexed) to a body-line
    /// position `(line_idx, display_col)` as a `BodyPos`. Returns
    /// `None` when the row falls past the last body line (footer
    /// area, or the empty strip below content in early-session
    /// views) — used by `begin_selection` to refuse to anchor a
    /// selection in the footer.
    fn screen_to_body(&self, col: u16, row: u16) -> Option<crate::render::selection::BodyPos> {
        let body_height = self.body_height() as usize;
        if (row as usize) >= body_height {
            return None;
        }
        let total = self.body_lines.len();
        if total == 0 {
            return None;
        }
        let viewport_start = if self.sticky_bottom {
            total.saturating_sub(body_height)
        } else {
            self.viewport_top.min(total.saturating_sub(body_height))
        };
        let line_idx = viewport_start + row as usize;
        if line_idx >= total {
            return None;
        }
        Some((line_idx, col))
    }

    /// Return the selected text (CSI-stripped, lines joined with `\n`),
    /// or an empty string when no selection exists or covers no visible
    /// chars. Delegates to the shared selection module's helpers rather
    /// than duplicating the walk logic here. Used only by tests.
    #[cfg(test)]
    fn extract_selection_text(&self) -> String {
        let Some(sel) = self.selection.selection else {
            return String::new();
        };
        let (lo_pos, hi_pos) = if sel.anchor <= sel.head {
            (sel.anchor, sel.head)
        } else {
            (sel.head, sel.anchor)
        };
        let lo = (lo_pos.0, lo_pos.1 as usize);
        let hi = (hi_pos.0, hi_pos.1 as usize);
        let total = self.body_lines.len();
        if lo.0 >= total {
            return String::new();
        }
        let mut parts = Vec::with_capacity(hi.0 - lo.0 + 1);
        for line_idx in lo.0..=hi.0.min(total - 1) {
            let line = &self.body_lines[line_idx];
            let Some((s, e)) =
                selection_col_range_for_line(line_idx, lo, hi, line)
            else {
                parts.push(String::new());
                continue;
            };
            parts.push(extract_line_selection_text(line, s, e));
        }
        parts.join("\n")
    }

/// Paint the footer strip. Layout (top to bottom, 1-indexed rows
    /// computed from the bottom of the viewport):
    ///   spinner       (1 row, blank when no streaming)
    ///   top rule      (1 row, full-width cyan ─)
    ///   input         (1 row, `❯ {buf}` flush-left)
    ///   bot rule      (1 row, full-width cyan ─)
    ///   menu items    (0..4 rows, when slash palette is active)
    ///   status        (1 row, dim `model · cwd`)
    ///
    /// Mirrors retained's footer shape (see `RetainedRenderer::paint_footer`)
    /// minus the wrapped multi-line input — alt-screen Phase 4 keeps
    /// input single-line; multi-line input is a Phase 5+ enhancement.
    fn paint_footer(&mut self) {
        if !self.footer_dirty {
            return;
        }
        let h = self.height;
        let total_footer = self.footer_rows();
        let footer_top = h.saturating_sub(total_footer) + 1; // 1-indexed
        let menu_rows = self.menu_paint_rows();
        let attachment_rows = self.pending_attachments.len() as u16;
        let spinner_row = footer_top;
        let top_rule_row = footer_top + 1;
        let input_row = footer_top + 2;
        let bot_rule_row = footer_top + 3;
        // Attachment preview rows (`└ [Image #N]`) sit between the
        // bot_rule and the menu — same slot the retained renderer uses
        // (see `RetainedRenderer::paint_footer`). Count is variable so
        // menu_first_row / status_row shift accordingly.
        let attach_first_row = footer_top + 4;
        let menu_first_row = footer_top + 4 + attachment_rows;
        let status_row = footer_top + 4 + attachment_rows + menu_rows;

        // Row 1 of footer: spinner during streaming, blank otherwise.
        // Frame glyph in brand magenta (Role::Brand) supplies the
        // visual anchor; label is bold + default-fg, mirroring
        // retained's `style_bold(Role::Secondary)` in
        // `build_spinner_body_row`. SGR_DIM was the prior choice but
        // rendered as hard-to-read mid-gray on Windows cmd (legacy
        // conhost <1809 swallowed the dim attribute, leaving the
        // label barely visible against the background).
        let cup = format!("\x1b[{};1H\x1b[K", spinner_row);
        let _ = self.out.write_all(cup.as_bytes());
        if let Some((frame, label)) = &self.pending_spinner {
            let cleaned = scrub_controls(label);
            let line = if self.caps.colors {
                format!(
                    "{}{}{} {}{}{}",
                    SGR_MAGENTA, frame, SGR_RESET, SGR_BOLD, cleaned, SGR_RESET
                )
            } else {
                format!("{} {}", frame, cleaned)
            };
            let _ = self.out.write_all(line.as_bytes());
        }

        // Top rule: full-width cyan ━ above the input box. Mirrors
        // retained's `build_rule_row`.
        //
        // U+2501 (━ HEAVY HORIZONTAL) instead of U+2500 (─ LIGHT
        // HORIZONTAL): on legacy Windows conhost with the default
        // Consolas / Lucida Console fonts the light variant renders
        // with visible vertical gaps between cells (the glyph stroke
        // doesn't span the full cell width), so the rule reads as a
        // dashed line even at full brightness. The heavy variant has
        // a thicker stroke that fills the cell, eliminating the
        // dashed look while still living in the same Box Drawing
        // block (every modern terminal + conhost-with-unicode-font
        // supports it). Bright cyan alone was insufficient — the
        // gap between glyphs persisted regardless of colour. See
        // commit fcf6a7e for the prior dim→bright attempt.
        //
        // No ASCII fallback: U+2501 is in WGL4, present on every
        // Windows monospace font (Consolas, NSimSun, Cascadia,
        // Microsoft YaHei). Falling back to `-` here on legacy conhost
        // produced a literal hyphen-dotted line that users read as
        // "broken/dashed border" — exactly what the heavy variant was
        // chosen to avoid.
        let rule = "\u{2501}".repeat(self.width as usize);
        let cup = format!("\x1b[{};1H\x1b[K", top_rule_row);
        let _ = self.out.write_all(cup.as_bytes());
        if self.caps.colors {
            let _ = write!(self.out, "{}{}{}", SGR_CYAN, rule, SGR_RESET);
        } else {
            let _ = self.out.write_all(rule.as_bytes());
        }

        // Session-name pill overlay: ` {name} ` painted in reverse +
        // cyan (cyan bg, terminal default fg) over the right end of
        // the top rule. Mirrors CC's per-conversation badge so the
        // user can tell which session they're typing into at a
        // glance. Only emitted when `session_name` is Some — populated
        // by build_status iff `Session::user_renamed`, so auto-named
        // sessions stay badge-less.
        //
        // Layout budget:
        //   right_margin   = 2 cells (don't hug the rightmost column —
        //                    legacy conhost wraps when writing into the
        //                    last cell of a row)
        //   pill_padding   = 2 cells (one space each side of the name)
        //   min_rule_left  = 8 cells (keep enough ━ on the left so the
        //                    box still reads as bordered; without this
        //                    a very long name eats the entire rule and
        //                    the input box loses its visual anchor)
        // Available for the name: width - right_margin - pill_padding
        //                         - min_rule_left = width - 12.
        // If the terminal is too narrow for even 1 cell of name + the
        // surrounding chrome, skip the badge entirely.
        if let Some(name_raw) = self.pending_status.session_name.as_ref() {
            let name_scrubbed = scrub_controls(name_raw);
            const RIGHT_MARGIN: usize = 2;
            const PILL_PADDING: usize = 2;
            const MIN_RULE_LEFT: usize = 8;
            let total_w = self.width as usize;
            let chrome = RIGHT_MARGIN + PILL_PADDING + MIN_RULE_LEFT;
            if total_w > chrome {
                let max_name_w = total_w - chrome;
                let name_w = crate::width::display_width(&name_scrubbed);
                // Truncate with `…` when over budget. `…` is one cell;
                // `truncate_to_width(name, max_name_w - 1) + "…"` keeps
                // the total under max_name_w. If max_name_w == 1, just
                // emit a single `…` (no room for any name char).
                let name_for_pill = if name_w <= max_name_w {
                    name_scrubbed
                } else if max_name_w <= 1 {
                    "…".to_string()
                } else {
                    let truncated = crate::width::truncate_to_width(&name_scrubbed, max_name_w - 1);
                    format!("{}…", truncated)
                };
                let pill_content_w = crate::width::display_width(&name_for_pill) + PILL_PADDING;
                // Start column (1-indexed) so the pill ends RIGHT_MARGIN
                // cells from the rightmost column. e.g. width=80,
                // pill_content_w=12, margin=2 → start col = 80-2-12+1 = 67.
                let start_col = total_w.saturating_sub(RIGHT_MARGIN + pill_content_w) + 1;
                let cup = format!("\x1b[{};{}H", top_rule_row, start_col);
                let _ = self.out.write_all(cup.as_bytes());
                if self.caps.colors {
                    // SGR for the pill differs by theme. Dark: reverse +
                    // bright cyan (SGR 7;96) — bright cyan as background
                    // pops against the dark default fg. Light: bold +
                    // standard magenta (SGR 1;35), no reverse — standard
                    // magenta maps to a dark, readable shade on light
                    // profiles, where bright-cyan reverse turned into
                    // pale-aqua-on-white and the chip vanished into the
                    // surrounding background.
                    let sgr = if crate::highlight::theme::is_light_for_render() {
                        "\x1b[1;35m"
                    } else {
                        "\x1b[7;96m"
                    };
                    let _ = write!(self.out, "{} {} \x1b[0m", sgr, name_for_pill);
                } else {
                    // No colors: surround with spaces so the name stays
                    // legible against the ━ rule on either side.
                    let _ = write!(self.out, " {} ", name_for_pill);
                }
            }
        }

        // Input row: `❯ {buf}` flush-left at col 0. matches retained's
        // `build_middle_row`.
        let cup = format!("\x1b[{};1H\x1b[K", input_row);
        let _ = self.out.write_all(cup.as_bytes());
        let chev = self.caps.prompt_chevron();
        let buf_str = self.pending_input.as_ref().map(|(b, _)| b.as_str()).unwrap_or("");
        let cursor_byte = self
            .pending_input
            .as_ref()
            .map(|(_, c)| (*c).min(buf_str.len()))
            .unwrap_or(0);
        // Show `\n` as a visible marker so users typing `\<Enter>` (the
        // line-continuation escape, used when Shift/Alt+Enter are
        // swallowed by the host terminal — typical on Windows
        // cmd.exe / legacy conhost without Kitty keyboard protocol)
        // get visual feedback that the newline was inserted.
        // Replacing with a plain space made the input box render
        // `abc def` regardless of whether the user typed a space or
        // `\<Enter>`, so users on Windows cmd reported "shift+enter
        // / alt+enter / \<Enter> 都无法换行" — they had no UI signal
        // that `\<Enter>` actually worked. `↵` (U+21B5) is one
        // display cell in modern fonts; ASCII fallback uses two
        // chars `\n` so the marker stays readable on legacy conhost
        // with NSimSun.
        let nl_marker = if self.caps.unicode_symbols {
            "↵"
        } else {
            "\\n"
        };
        let safe_buf = scrub_controls(buf_str).replace('\n', nl_marker);
        let max_cols = (self.width as usize).saturating_sub(chev.chars().count());
        // Display column of the cursor *within* `safe_buf`, computed
        // with the SAME `\n → nl_marker` substitution as the rendered
        // line. The previous implementation replaced `\n` with a single
        // space here while the rendered line used `\\n` (two cols on
        // legacy conhost without unicode-capable fonts), so every
        // newline in the buffer slid the cursor one column to the left
        // of where the user could see they were typing.
        let prefix_safe = scrub_controls(&buf_str[..cursor_byte]).replace('\n', nl_marker);
        let cursor_col_in_buf = display_width(&prefix_safe);
        // Horizontal scroll: when the user types past `max_cols` (or
        // moves the cursor past it), slide the visible window so the
        // cursor stays at the right edge instead of falling off.
        // Without this, `truncate_to_width(&safe_buf, max_cols)` kept
        // only the leading window and the user's recent typing simply
        // disappeared — they reported "input box gets too long, can't
        // see what I'm typing anymore". The window ends at the cursor
        // (cursor visible at the rightmost col); if the cursor is in
        // the early portion of the buffer, no scrolling kicks in and
        // we render the head as before.
        let (trimmed, visible_cursor_col) = if cursor_col_in_buf < max_cols {
            (truncate_to_width(&safe_buf, max_cols), cursor_col_in_buf)
        } else {
            let start_col = cursor_col_in_buf + 1 - max_cols;
            (
                crate::width::slice_cols(&safe_buf, start_col, max_cols),
                max_cols.saturating_sub(1),
            )
        };
        let input_line = if self.caps.colors {
            format!("{}{}{}{}", SGR_CYAN, chev, SGR_RESET, trimmed)
        } else {
            format!("{}{}", chev, trimmed)
        };
        let _ = self.out.write_all(input_line.as_bytes());

        // Bottom rule: same as top rule.
        let cup = format!("\x1b[{};1H\x1b[K", bot_rule_row);
        let _ = self.out.write_all(cup.as_bytes());
        if self.caps.colors {
            let _ = write!(self.out, "{}{}{}", SGR_CYAN, rule, SGR_RESET);
        } else {
            let _ = self.out.write_all(rule.as_bytes());
        }

        // Attachment preview rows — `└ [Image #N]` in dim/muted style.
        // Pre-filtered upstream (see `event_loop::compute_input_attachments`)
        // to only include marker numbers whose bytes are actually pending,
        // so showing a row is a real visual confirmation that an image is
        // attached (not just literal `[Image #N]` text the user typed).
        // Mirrors the post-submit muted echo of the same string in the
        // body, so users see a consistent look pre- and post-submit.
        for (i, n) in self.pending_attachments.iter().enumerate() {
            let row_n = attach_first_row + i as u16;
            let cup = format!("\x1b[{};1H\x1b[K", row_n);
            let _ = self.out.write_all(cup.as_bytes());
            let line = format!("  \u{2514} [Image #{}]", n);
            if self.caps.colors {
                let _ = write!(self.out, "{}{}{}", SGR_DIM, line, SGR_RESET);
            } else {
                let _ = self.out.write_all(line.as_bytes());
            }
        }

        // Menu rows: 0..4 of `/{name}  {desc}`. Selected gets `▸` prefix
        // + reverse-video for visibility. Pagination around `selected`
        // (matches retained's 4-item viewport) so a 50-command match
        // list doesn't crowd the screen.
        if let Some(menu) = self.pending_menu.clone() {
            let len = menu.items.len();
            let offset = if len <= 4 {
                0
            } else if menu.selected < 4 {
                0
            } else {
                (menu.selected + 1).saturating_sub(4).min(len.saturating_sub(4))
            };
            let end = (offset + 4).min(len);
            for (i, (name, desc)) in menu.items[offset..end].iter().enumerate() {
                let row_n = menu_first_row + i as u16;
                let cup = format!("\x1b[{};1H\x1b[K", row_n);
                let _ = self.out.write_all(cup.as_bytes());
                let selected = (offset + i) == menu.selected;
                let safe_name = scrub_controls(name);
                let safe_desc = scrub_controls(desc);
                let body = match menu.kind {
                    crate::render::MenuKind::SlashCommand => {
                        // Pad by DISPLAY width, not char count: `/设为默认`
                        // (5 chars, 9 cells) needs the same description
                        // start column as `/添加` (3 chars, 5 cells). The
                        // previous `{:<12}` char-count padding left CJK
                        // rows two cells to the right of ASCII rows.
                        let name_width = unicode_width::UnicodeWidthStr::width(safe_name.as_str());
                        let pad = 12usize.saturating_sub(name_width);
                        let padded = format!("{}{}", safe_name, " ".repeat(pad));
                        if selected {
                            format!("▸ /{}  {}", padded, safe_desc)
                        } else {
                            format!("  /{}  {}", padded, safe_desc)
                        }
                    }
                    crate::render::MenuKind::AtMention => {
                        // No leading whitespace — `+` flush left.
                        if safe_desc.is_empty() {
                            format!("+ {}", safe_name)
                        } else {
                            format!("+ {}  {}", safe_name, safe_desc)
                        }
                    }
                };
                // Clamp to terminal width before write. Without this,
                // long descriptions (CJK glyphs are 2 display cells)
                // overflow and the terminal auto-wraps onto subsequent
                // rows. Single-row wrap is wiped by the next iteration's
                // CUP+EL, but a 2+ row wrap leaks past that recovery
                // and leaves stale glyphs in column 1+ of later menu
                // items — observed on plugin skill listings with very
                // long Chinese descriptions.
                let body = truncate_to_width(&body, self.width as usize);
                if self.caps.colors {
                    if selected {
                        // Reverse video on the selected row to make
                        // the keyboard focus highly visible.
                        let _ = write!(self.out, "\x1b[7m{}\x1b[0m", body);
                    } else {
                        let _ = write!(self.out, "{}{}{}", SGR_DIM, body, SGR_RESET);
                    }
                } else {
                    let _ = self.out.write_all(body.as_bytes());
                }
            }
        }

        // Status row at the bottom: dim `model · cwd`, optionally
        // prefixed by a brand-colored `PLAN` mode badge so non-default
        // agent modes are visible at a glance (mirrors retained's
        // build_status_row treatment).
        let cup = format!("\x1b[{};1H\x1b[K", status_row);
        let _ = self.out.write_all(cup.as_bytes());
        let mode_badge = self
            .pending_status
            .mode_indicator
            .as_ref()
            .map(|s| scrub_controls(s));
        // Pre-truncate cwd so it does not overflow the terminal width.
        // Compute a budget for cwd that accounts for model name, " · "
        // separators, and mode badge — same logic as retained's
        // build_status_row.
        let model = scrub_controls(&self.pending_status.model);
        let cwd_full = scrub_controls(&self.pending_status.cwd);
        let mode_badge_w = mode_badge
            .as_ref()
            .map(|s| crate::width::display_width(s) + 1)
            .unwrap_or(0);
        let sep_w = if !model.is_empty() && !cwd_full.is_empty() { 3 } else { 0 };
        let left_max = (self.width as usize).saturating_sub(mode_badge_w);
        let cwd_budget = left_max
            .saturating_sub(crate::width::display_width(&model))
            .saturating_sub(sep_w);
        let cwd = if !cwd_full.is_empty() && cwd_budget > 0
            && crate::width::display_width(&cwd_full) > cwd_budget
        {
            crate::width::truncate_path(&cwd_full, cwd_budget)
        } else if !cwd_full.is_empty() && cwd_budget == 0 {
            crate::width::truncate_path(&cwd_full, left_max)
        } else {
            cwd_full
        };
        let status_text = if !model.is_empty() || !cwd.is_empty() {
            if model.is_empty() {
                format!("  {}", cwd)
            } else if cwd.is_empty() {
                format!("  {}", model)
            } else {
                format!("  {} \u{00b7} {}", model, cwd)
            }
        } else {
            String::new()
        };
        if mode_badge.is_some() || !status_text.is_empty() {
            // Badge gets brand-colored magenta (Role::Brand). Status
            // body keeps its faint/dim style. Color codes only emit
            // when the terminal advertises color support.
            if let Some(badge) = &mode_badge {
                if self.caps.colors {
                    let _ = write!(self.out, "  {}{}{} ", SGR_MAGENTA, badge, SGR_RESET);
                } else {
                    let _ = write!(self.out, "  {} ", badge);
                }
            }
            if !status_text.is_empty() {
                // status_text already includes its own leading 2-space pad
                // when no badge precedes it. With a badge we already
                // emitted the leading spaces + badge + space, so trim
                // the duplicate leading pad to keep alignment.
                let body = if mode_badge.is_some() {
                    status_text.trim_start_matches(' ').to_string()
                } else {
                    status_text
                };
                let line = if self.caps.colors {
                    format!("{}{}{}", SGR_DIM, body, SGR_RESET)
                } else {
                    body
                };
                let _ = self.out.write_all(line.as_bytes());
            }
        }

        // Position the terminal cursor inside the input row so the
        // user sees where their typing will land. `visible_cursor_col`
        // is the cursor's column *within the visible window* — already
        // accounts for both the `\n → nl_marker` rendering and any
        // horizontal scroll (when the buffer overflowed `max_cols` and
        // we slid the window so the cursor stays at the right edge).
        // Adding `chev.chars().count()` skips past the prompt glyph;
        // the `+ 1` converts to the 1-indexed CSI CUP coordinate.
        if self.pending_input.is_some() {
            let cursor_col = chev.chars().count() + visible_cursor_col;
            if self.cursor_shown {
                // Cursor already visible from a prior frame — just
                // reposition it. Skipping the `?25h` re-emit avoids
                // restarting the host terminal's hardware cursor blink
                // animation, which on macOS Terminal.app at streaming
                // framerate reads as constant flicker.
                let cup = format!("\x1b[{};{}H", input_row, cursor_col + 1);
                let _ = self.out.write_all(cup.as_bytes());
            } else {
                let cup = format!("\x1b[{};{}H\x1b[?25h", input_row, cursor_col + 1);
                let _ = self.out.write_all(cup.as_bytes());
                self.cursor_shown = true;
            }
        } else if self.cursor_shown {
            let _ = self.out.write_all(b"\x1b[?25l");
            self.cursor_shown = false;
        }

        // No flush here: paint_frame's tail (anchor_cursor_to_input)
        // is the single flush point for the whole frame. Flushing
        // here gave the terminal a vsync window between footer
        // writes and anchor's final CUP, briefly showing the cursor
        // at the end of the status row before it jumped to input.
        // Tests call `r.flush()` explicitly when invoking
        // `paint_footer` directly.
        self.footer_dirty = false;
    }

    /// Combined frame paint: body first, footer second so the cursor
    /// final-position belongs to the footer (typically the input row).
    ///
    /// Cursor visibility handling depends on `slow_paint_terminal`:
    ///
    /// **Slow terminals (JediTerm, legacy conhost, `slow_paint_terminal=true`):**
    /// hide cursor before paint_body so its journey through every
    /// intermediate CUP isn't visible. paint_footer's tail re-emits
    /// show-cursor (`?25h`) at the final input-row position when
    /// `pending_input` is set. Without this, JediTerm rendered the
    /// cursor's trail as visible "jumping" — Android Studio bug.
    ///
    /// **Fast terminals (macOS Terminal.app / iTerm2 / xterm /
    /// WezTerm / Kitty, `slow_paint_terminal=false`):** leave cursor
    /// visible the whole time; just reposition via final CUP. The
    /// per-row CUPs DO still flash the cursor through body cells but
    /// they execute in well under a refresh interval so the trail is
    /// imperceptible. Avoiding the per-frame `?25l`/`?25h` toggle is
    /// what matters here — at streaming framerate (~30 Hz) that
    /// toggle reads as constant flicker on macOS Terminal.app even
    /// after `?12l` disabled the hardware cursor blink.
    fn paint_frame(&mut self) {
        if self.slow_paint_terminal && self.cursor_shown {
            let _ = self.out.write_all(b"\x1b[?25l");
            self.cursor_shown = false;
        }
        self.paint_body();
        self.paint_footer();
        // paint_body always leaves the cursor at the last body-row
        // it touched (CUP+EL+content per row); paint_footer's tail
        // only re-anchors the cursor when footer_dirty is true, so
        // a body-only render (e.g. async MCP "已连接" CommandOutput
        // after startup) leaves the visible cursor stranded mid-body
        // far from the input row. Re-anchor on every frame when an
        // input prompt is showing — the CUP is one cheap escape and
        // matches what fast terminals (macOS Terminal.app etc.)
        // expect: cursor visible AT the input column.
        self.anchor_cursor_to_input();
        // Single flush point for the whole frame. paint_body and
        // paint_footer intentionally skip their own flushes so the
        // terminal renders only the final cursor position, not the
        // intermediate body-row / status-row landings that produced
        // a visible cursor "blink" mid-screen during streaming on
        // macOS Terminal.app. anchor_cursor_to_input early-returns
        // (no flush) when pending_input is None — handle that here.
        let _ = self.out.flush();
    }

    /// Move the terminal cursor back to the input row's character
    /// position. Called at the end of every paint_frame so async
    /// body updates (which only set body_dirty and skip the footer
    /// repaint) don't leave the cursor stranded above the input
    /// box. No-op when no input prompt is active.
    fn anchor_cursor_to_input(&mut self) {
        let Some((buf_str, cursor_byte)) = self.pending_input.clone() else {
            return;
        };
        let total_footer = self.footer_rows();
        let footer_top = self.height.saturating_sub(total_footer) + 1;
        let input_row = footer_top + 2;
        let chev = self.caps.prompt_chevron();
        let chev_width = chev.chars().count();
        let max_cols = (self.width as usize).saturating_sub(chev_width);
        let cursor_byte = cursor_byte.min(buf_str.len());
        let nl_marker = if self.caps.unicode_symbols { "↵" } else { "\\n" };
        let prefix_safe = scrub_controls(&buf_str[..cursor_byte]).replace('\n', nl_marker);
        let cursor_col_in_buf = display_width(&prefix_safe);
        let visible_cursor_col = if cursor_col_in_buf < max_cols {
            cursor_col_in_buf
        } else {
            max_cols.saturating_sub(1)
        };
        let cursor_col = chev_width + visible_cursor_col;
        if self.cursor_shown {
            let cup = format!("\x1b[{};{}H", input_row, cursor_col + 1);
            let _ = self.out.write_all(cup.as_bytes());
        } else {
            let cup = format!("\x1b[{};{}H\x1b[?25h", input_row, cursor_col + 1);
            let _ = self.out.write_all(cup.as_bytes());
            self.cursor_shown = true;
        }
        let _ = self.out.flush();
    }

    /// Pipe one completed line through the markdown renderer and push
    /// the result. None outputs (table buffering, fence toggle) are
    /// dropped intentionally — the renderer handles flush via the
    /// next non-buffered line. Always-some output (the common case)
    /// becomes one body_lines entry.
    fn render_md_and_push(&mut self, line: &str) {
        // Pass terminal width through so markdown tables render in flat
        // mode when they don't fit at natural column widths (mirrors the
        // `RetainedRenderer` path). Alt-screen body has no left padding,
        // so the full screen width is the budget.
        let md_width = self.width as usize;
        if let Some(rendered) =
            crate::markdown::render_line_with_width(line, &mut self.md_state, self.caps, md_width)
        {
            // `rendered` may itself contain `\n` when it includes a
            // table flush prefix from a prior buffered block. Split
            // so each physical line becomes its own raw_body_lines entry
            // — `push_body_row_raw` handles soft-wrapping at terminal
            // width and stores the raw line for re-flow on resize
            // (issue #363).
            for sub in rendered.split('\n') {
                self.push_body_row_raw(sub.to_string());
            }
        }
    }

    /// Flush the in-progress assistant streaming buffer as a body row,
    /// regardless of whether a `\n` was seen. Called by
    /// `AssistantLineBreak`, `TurnComplete`, and any non-streaming
    /// UiLine that arrives mid-stream — locks in the partial chunk so
    /// it stays in scrollback rather than dangling.
    fn flush_assistant_remainder(&mut self) {
        if !self.assistant_line_buf.is_empty() {
            let line = std::mem::take(&mut self.assistant_line_buf);
            self.render_md_and_push(&line);
        }
        // Also flush any pending markdown state (e.g. a buffered
        // table block) so end-of-turn doesn't strand it. Mirrors
        // RetainedRenderer's TurnComplete handling.
        let md_width = self.width as usize;
        if let Some(tail) =
            crate::markdown::finalize_with_width(&mut self.md_state, self.caps, md_width)
        {
            for sub in tail.split('\n') {
                self.push_body_row_raw(sub.to_string());
            }
        }
    }

    /// Append streaming assistant text. Splits at `\n` so each completed
    /// physical line gets routed through the markdown renderer; partial
    /// trailing chunks stay in the buffer until the next `\n` or
    /// `AssistantLineBreak`. Inline markdown (`**bold**`, `*italic*`,
    /// `\`code\``) and block markdown (headings, code fences, tables) all
    /// resolve through `crate::markdown::render_line`.
    fn append_assistant_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.assistant_line_buf);
                self.render_md_and_push(&line);
            } else {
                self.assistant_line_buf.push(ch);
            }
        }
    }

    /// Build a horizontal-rule TurnSeparator like
    /// `─────── label ───────` centred on the terminal width. Mirrors
    /// the retained renderer's TurnSeparator rendering at a coarser
    /// grain (no Cell layout, just inline SGR). Dimmed via SGR 2 on
    /// the terminal-default fg so the rule reads as "quiet decoration"
    /// across light/dark themes — same contract as retained's
    /// `style_faint(Role::Secondary)`.
    fn build_turn_separator(&self, label: &str) -> String {
        let w = (self.width as usize).max(20);
        let label_text = format!(" {} ", scrub_controls(label));
        let label_w = label_text.chars().count();
        let remaining = w.saturating_sub(label_w);
        let left = remaining / 2;
        let right = remaining - left;
        let dashes_left = "─".repeat(left);
        let dashes_right = "─".repeat(right);
        if self.caps.colors {
            format!("{}{}{}{}{}", SGR_DIM, dashes_left, label_text, dashes_right, SGR_RESET)
        } else {
            format!("{}{}{}", dashes_left, label_text, dashes_right)
        }
    }

    /// Banner rows pushed for `UiLine::Welcome`. Mirrors retained's
    /// layout (see `RetainedRenderer::build_welcome_rows`):
    ///   ◆ AtomCode                           v… · MIT
    ///   · {working_dir}
    ///   · {model}
    ///   (blank)
    ///   type something, or press / to browse commands
    ///   /provider  to add a custom model
    ///   (blank)
    fn push_welcome(&mut self, model: &str, working_dir: &str) {
        let diamond = if self.caps.unicode_symbols { "\u{25c6}" } else { "*" };
        let bullet = if self.caps.unicode_symbols { "\u{2219}" } else { "*" };
        // Title row with right-aligned version + license. Fill the
        // gap with spaces so v4.x.y · MIT lands at the right edge.
        let version = format!("v{}", env!("CARGO_PKG_VERSION"));
        let licence = "MIT";
        let title_left = format!("{} AtomCode", diamond);
        let title_right = format!("{} \u{00b7} {}", version, licence);
        let title_left_w = display_width(&title_left);
        let title_right_w = display_width(&title_right);
        let total_w = self.width as usize;
        let gap = total_w
            .saturating_sub(title_left_w)
            .saturating_sub(title_right_w);
        let title = if self.caps.colors {
            format!(
                "{}{}{}{}{}{}{}",
                SGR_MAGENTA,
                title_left,
                SGR_RESET,
                " ".repeat(gap),
                SGR_DIM,
                title_right,
                SGR_RESET,
            )
        } else {
            format!("{}{}{}", title_left, " ".repeat(gap), title_right)
        };
        self.push_body_row(title);
        self.push_body_row(format!("{} {}", bullet, scrub_controls(working_dir)));
        self.push_body_row(format!("{} {}", bullet, scrub_controls(model)));
        self.push_body_row(String::new());
        // Onboarding hints: combine onto one row when the terminal is
        // wide enough; otherwise emit three rows. Mirrors the same
        // decision the retained renderer makes — push_body_row can't
        // reflow, so on narrow widths we'd otherwise overflow off the
        // right edge.
        let idle_full_w = display_width(&t(Msg::IdleHintFull));
        let provider_full_w = display_width(&t(Msg::IdleHintProviderFull));
        let codingplan_full_w = display_width(&t(Msg::IdleHintCodingplanFull));
        let combined_w = idle_full_w + 3 + provider_full_w + 3 + codingplan_full_w;
        if combined_w <= self.width as usize {
            let hint_line = if self.caps.colors {
                let hint_a = format!(
                    "{}{}{}{}{}{}{}{}{}",
                    SGR_DIM, t(Msg::IdleHintPrefix), SGR_RESET,
                    SGR_CYAN, t(Msg::IdleHintSlash), SGR_RESET,
                    SGR_DIM, t(Msg::IdleHintSuffix), SGR_RESET,
                );
                let hint_b = format!(
                    "{}{}{}  {}{}{}",
                    SGR_CYAN, t(Msg::IdleHintProvider), SGR_RESET,
                    SGR_DIM, t(Msg::IdleHintProviderSuffix), SGR_RESET,
                );
                let hint_c = format!(
                    "{}{}{}  {}{}{}",
                    SGR_CYAN, t(Msg::IdleHintCodingplan), SGR_RESET,
                    SGR_DIM, t(Msg::IdleHintCodingplanSuffix), SGR_RESET,
                );
                format!("{}   {}   {}", hint_a, hint_b, hint_c)
            } else {
                format!(
                    "{}   {}   {}",
                    t(Msg::IdleHintFull),
                    t(Msg::IdleHintProviderFull),
                    t(Msg::IdleHintCodingplanFull),
                )
            };
            self.push_body_row(hint_line);
        } else {
            let hint_a = if self.caps.colors {
                format!(
                    "{}{}{}{}{}{}{}{}{}",
                    SGR_DIM, t(Msg::IdleHintPrefix), SGR_RESET,
                    SGR_CYAN, t(Msg::IdleHintSlash), SGR_RESET,
                    SGR_DIM, t(Msg::IdleHintSuffix), SGR_RESET,
                )
            } else {
                t(Msg::IdleHintFull).into_owned()
            };
            self.push_body_row(hint_a);
            let hint_b = if self.caps.colors {
                format!(
                    "{}{}{}  {}{}{}",
                    SGR_CYAN, t(Msg::IdleHintProvider), SGR_RESET,
                    SGR_DIM, t(Msg::IdleHintProviderSuffix), SGR_RESET,
                )
            } else {
                t(Msg::IdleHintProviderFull).into_owned()
            };
            self.push_body_row(hint_b);
            let hint_c = if self.caps.colors {
                format!(
                    "{}{}{}  {}{}{}",
                    SGR_CYAN, t(Msg::IdleHintCodingplan), SGR_RESET,
                    SGR_DIM, t(Msg::IdleHintCodingplanSuffix), SGR_RESET,
                )
            } else {
                t(Msg::IdleHintCodingplanFull).into_owned()
            };
            self.push_body_row(hint_c);
        }
        self.push_body_row(String::new());
    }

    /// User echo row: `❯ {text}` (or `> {text}` on dumb caps) + blank
    /// spacer. Multi-line input (`\<Enter>` line-continuation,
    /// Shift/Alt+Enter on terminals that disambiguate, paste with
    /// embedded newlines) splits each physical line into its own
    /// body row — `paint_body` CUPs every body line to a distinct
    /// terminal row, so a single body string with embedded `\n`
    /// would corrupt the alt-screen layout: the literal LF in raw
    /// mode advances row but not column, then the next paint_body
    /// iteration CUP+EL-erases whatever landed below. Windows cmd
    /// users reported "abc<\><Enter>def" submitted as echo only
    /// showed `❯ abc`, the `def` flashed and disappeared.
    /// Continuation lines indent under the chevron-and-space prefix
    /// so multi-line user messages read as one paragraph rather than
    /// orphaned rows.
    fn push_user(&mut self, text: &str) {
        self.flush_assistant_remainder();
        self.md_state.reset();
        let chev = self.caps.prompt_chevron();
        let safe = scrub_controls(text);
        let chev_w = crate::width::display_width(chev);
        let cont_pad: String = " ".repeat(chev_w);
        for (i, line) in safe.split('\n').enumerate() {
            let row = if i == 0 {
                if self.caps.colors {
                    format!("{}{}{}{}", SGR_CYAN, chev, SGR_RESET, line)
                } else {
                    format!("{}{}", chev, line)
                }
            } else {
                format!("{}{}", cont_pad, line)
            };
            // Use push_body_row_raw so long user lines are soft-wrapped
            // and can be re-flowed on resize (issue #363).
            self.push_body_row_raw(row);
        }
        self.push_body_row(String::new());
    }

    /// `▸ name(detail)` row for tool calls. Cyan name when colours on.
    /// Same line for both `ToolCall` (terminal final-state) and
    /// `ToolCallInFlight` (Phase 2: no live spinner — stays static
    /// until commit). Spinner animation for in-flight ships in Phase 3.
    fn push_tool_call(&mut self, name: &str, detail: &str) {
        self.flush_assistant_remainder();
        // ● (U+25CF) — Geometric Shapes block, broadly available
        // across Windows monospace fonts. Was ▸ (U+25B8) but rendered
        // as `□` tofu on Windows VSCode/cmd.exe defaults; see the
        // matching comment in retained.rs ToolCall arm for the
        // Windows-font rationale.
        let arrow = "\u{25cf}";
        let name_safe = scrub_controls(name);
        let detail_safe = scrub_controls(detail);
        let row = match (self.caps.colors, detail_safe.is_empty()) {
            (true, true) => format!("{}{} {}{}", SGR_CYAN, arrow, name_safe, SGR_RESET),
            (true, false) => format!(
                "{}{} {}{}({})",
                SGR_CYAN, arrow, name_safe, SGR_RESET, detail_safe
            ),
            (false, true) => format!("{} {}", arrow, name_safe),
            (false, false) => format!("{} {}({})", arrow, name_safe, detail_safe),
        };
        // Use push_body_row_raw so long tool-call lines are soft-wrapped
        // and can be re-flowed on resize (issue #363).
        self.push_body_row_raw(row);
    }

    /// `✓ summary` (green) or `✗ summary` (red) row. PlainRenderer-style.
    fn push_tool_result(&mut self, success: bool, summary: &str) {
        self.flush_assistant_remainder();
        let icon = if success { "\u{2713}" } else { "\u{2717}" }; // ✓ ✗
        let safe = scrub_controls(summary);
        let row = if self.caps.colors {
            let color = if success { SGR_GREEN } else { SGR_RED };
            format!("    {}{}{} {}", color, icon, SGR_RESET, safe)
        } else {
            format!("    {} {}", icon, safe)
        };
        // Use push_body_row_raw so long tool-result lines are soft-wrapped
        // and can be re-flowed on resize (issue #363).
        self.push_body_row_raw(row);
    }

    /// `[Error: ...]` row. Red when colours on. Mirrors PlainRenderer.
    fn push_error(&mut self, msg: &str) {
        self.flush_assistant_remainder();
        let safe = scrub_controls(msg);
        let label = t(Msg::ErrorPrefix { msg: &safe });
        let row = if self.caps.colors {
            format!("{}{}{}", SGR_RED, label, SGR_RESET)
        } else {
            label.into_owned()
        };
        // Use push_body_row_raw so long error lines are soft-wrapped
        // and can be re-flowed on resize (issue #363).
        self.push_body_row_raw(row);
    }

    fn push_warning(&mut self, msg: &str) {
        self.flush_assistant_remainder();
        let safe = scrub_controls(msg);
        // Bold yellow `! …` advisory. Visually softer than the red
        // [Error: …] but still high-contrast — meant to be impossible
        // to scroll past without noticing.
        let row = if self.caps.colors {
            format!("\x1b[1;33m! {}{}", safe, SGR_RESET)
        } else {
            format!("! {}", safe)
        };
        // Use push_body_row_raw so long warning lines are soft-wrapped
        // and can be re-flowed on resize (issue #363).
        self.push_body_row_raw(row);
    }

    /// Push `text` as command-output rows wrapped in `sgr_open` (e.g.
    /// `SGR_GREY` or `SGR_BOLD`). Scrubs first, soft-wraps each line,
    /// THEN paints SGR around every wrapped chunk — this ordering is
    /// load-bearing: `push_command_output` runs `scrub_controls` which
    /// would otherwise strip caller-supplied SGR if the styling were
    /// applied first. Used for ToolGroup header (bold) and child
    /// rows (muted gray), mirroring retained's role-based styling.
    fn push_styled_command_output(&mut self, text: &str, sgr_open: &str) {
        self.flush_assistant_remainder();
        let safe = scrub_controls(text);
        let style_on = self.caps.colors && !sgr_open.is_empty();
        for line in safe.split('\n') {
            let row = if style_on {
                format!("{}{}{}", sgr_open, line, SGR_RESET)
            } else {
                line.to_string()
            };
            // Use push_body_row_raw so long styled output lines are
            // soft-wrapped and can be re-flowed on resize (issue #363).
            self.push_body_row_raw(row);
        }
    }

    /// `(cancelled)` marker row.
    fn push_cancelled(&mut self) {
        self.flush_assistant_remainder();
        let label = t(Msg::Cancelled);
        let row = if self.caps.colors {
            format!("{}{}{}", SGR_DIM, label, SGR_RESET)
        } else {
            label.into_owned()
        };
        // Soft-wrap at terminal width (issue #363) — cancelled label
        // is typically short, but wrap for consistency and re-flow on
        // resize.
        self.push_body_row_raw(row);
    }

    /// Diff line: `+ added` (green) or `- removed` (red). Per-row sign.
    fn push_diff_line(&mut self, added: bool, text: &str) {
        let safe = scrub_controls(text);
        let row = match (self.caps.colors, added) {
            (true, true) => format!("    {}+ {}{}", SGR_GREEN, safe, SGR_RESET),
            (true, false) => format!("    {}- {}{}", SGR_RED, safe, SGR_RESET),
            (false, true) => format!("    + {}", safe),
            (false, false) => format!("    - {}", safe),
        };
        // Use push_body_row_raw so long diff lines are soft-wrapped
        // and can be re-flowed on resize (issue #363).
        self.push_body_row_raw(row);
    }

    /// Push CommandOutput verbatim, splitting on newlines so each
    /// physical line is its own body row.
    fn push_command_output(&mut self, text: &str) {
        self.flush_assistant_remainder();
        // CommandOutput is trusted internal text (slash-command
        // responses, setup reports, status echoes) — let SGR
        // through so things like the `/codingplan` red locked-model
        // row reach the terminal. Cursor moves, OSC, and other
        // potentially-dangerous escapes are still stripped. The
        // wrap helper inside push_body_row_raw is SGR-aware so
        // colour state survives line wrapping intact.
        let safe = crate::sanitize::scrub_controls_keep_sgr(text);
        // Use push_body_row_raw so long command-output lines are
        // soft-wrapped and can be re-flowed on resize (issue #363).
        for line in safe.split('\n') {
            self.push_body_row_raw(line.to_string());
        }
    }

    /// Jump the body viewport to an absolute line index, clamping to
    /// max_top. Used by the scroll_to_prev/next_message family.
    fn scroll_body_to(&mut self, target: usize) {
        let body_height = self.body_height() as usize;
        let max_top = self.body_lines.len().saturating_sub(body_height);
        self.viewport_top = target.min(max_top);
        self.sticky_bottom = self.viewport_top >= max_top;
        self.body_dirty = true;
        self.footer_dirty = true;
        self.paint_frame();
    }
}


impl<W: Write + Send> Renderer for AltScreenRenderer<W> {
    fn render(&mut self, line: UiLine) {
        match line {
            // ── body: welcome / turn events ──
            UiLine::Welcome { model, working_dir } => {
                self.push_welcome(&model, &working_dir);
            }
            UiLine::User(text) => {
                self.mark_message(crate::render::MarkKind::User);
                self.last_mark_was_assistant = false;
                self.push_user(&text);
            }
            UiLine::TurnSeparator { label } => {
                self.last_mark_was_assistant = false;
                let row = self.build_turn_separator(&label);
                self.push_body_row(String::new());
                self.push_body_row(row);
                self.push_body_row(String::new());
            }
            UiLine::TurnComplete => {
                self.flush_assistant_remainder();
            }
            UiLine::TurnCancelled => {
                self.push_cancelled();
            }

            // ── body: streaming assistant ──
            UiLine::AssistantText(text) => {
                if !self.last_mark_was_assistant {
                    self.mark_message(crate::render::MarkKind::Assistant);
                    self.last_mark_was_assistant = true;
                }
                self.append_assistant_text(&text);
            }
            UiLine::ReasoningText(text) => {
                // Dim styling for reasoning chunks; same SGR pattern
                // RetainedRenderer / PlainRenderer already use.
                if self.caps.colors {
                    let dimmed = format!("{}{}{}", SGR_DIM, scrub_controls(&text), SGR_RESET);
                    self.append_assistant_text(&dimmed);
                } else {
                    self.append_assistant_text(&text);
                }
            }
            UiLine::AssistantLineBreak => {
                self.flush_assistant_remainder();
            }

            // ── body: tools & diffs ──
            UiLine::ToolCall { name, detail }
            | UiLine::ToolCallInFlight { name, detail, .. } => {
                self.mark_message(crate::render::MarkKind::ToolCall);
                self.last_mark_was_assistant = false;
                self.push_tool_call(&name, &detail);
            }
            UiLine::ToolCallCommit { .. } => {
                // Phase 3 will add live-spinner freezing here. Phase 2
                // pushes ToolCallInFlight as a static row already, so
                // there's nothing to freeze yet.
            }
            UiLine::ToolGroupRender { batch_id: _, header, children } => {
                // Mark batch header as ToolCall anchor for Alt+↑/↓
                // message-jump (parity with retained).
                self.mark_message(crate::render::MarkKind::ToolCall);
                self.last_mark_was_assistant = false;
                // alt-screen mirrors retained's append-style without
                // the in-place ✓ rewrite (alt-screen layout is
                // virtual-buffer based; live-group rewrite would need
                // its own row tracking). Header + children print
                // statically; ChildUpdate appends a new row.
                //
                // Style parity with retained (`UiLine::ToolGroupRender`
                // arm in retained.rs):
                //   - header: bold, default fg — emphasises the
                //     `● Running N tools in parallel` anchor row
                //   - children: muted gray (SGR 90) — high-frequency
                //     per-call rows (`▸ bash(cmd…)`) that should read
                //     as subordinate detail, not compete with the
                //     header. User reported children rendered in
                //     default fg here, so the visual hierarchy was
                //     flattened relative to retained.
                self.push_styled_command_output(&header, SGR_BOLD);
                for c in children {
                    self.push_styled_command_output(&c.text, SGR_GREY);
                }
            }
            UiLine::ToolGroupChildUpdate { batch_id: _, call_id: _, new_text } => {
                // Update inherits the muted child styling so the row
                // stays visually subordinate after the result lands.
                self.push_styled_command_output(&new_text, SGR_GREY);
            }
            UiLine::ToolGroupSummary { text } => {
                // Mark batch summary as ToolResult anchor (parity with
                // retained). Without it Alt+↑/↓ skips the whole batch.
                self.mark_message(crate::render::MarkKind::ToolResult);
                self.last_mark_was_assistant = false;
                // Summary mirrors header: bold default-fg anchor row
                // closing the group. Matches retained's
                // `style_bold(Role::Secondary)` choice.
                self.push_styled_command_output(&text, SGR_BOLD);
            }
            UiLine::ToolResult { success, summary } => {
                self.mark_message(crate::render::MarkKind::ToolResult);
                self.last_mark_was_assistant = false;
                self.push_tool_result(success, &summary);
            }
            UiLine::DiffLine { added, text } => {
                self.push_diff_line(added, &text);
            }
            UiLine::DiffBlock(entries) => {
                for entry in entries {
                    self.push_diff_line(entry.added, &entry.text);
                }
            }
            UiLine::ApprovalPrompt { tool, detail } => {
                // Mirror retained's chip-based prompt: bold-yellow
                // "▶ Waiting for approval:" label + Y/A/N reverse-video
                // chips (green / cyan / red) + their textual labels.
                // The previous alt-screen path emitted the flat
                // `ApprovalPromptAlt` sentence — visually indistinct from
                // a regular command-output row, so users couldn't tell at
                // a glance that an approval was pending.
                //
                // Include the tool name and detail so the user knows which
                // specific action they're being asked to approve. Without
                // this, parallel batch approvals (e.g. 3 × Read) show
                // identical prompts and the user can't tell which file
                // they're approving (issue #439).
                //
                // When the label + chips fit on one line, place them
                // together (issue #454: users reported unnecessary
                // line-splitting). Only split when the label is too long.
                let waiting = t(Msg::ApprovalWaitingLabel);
                let allow = t(Msg::ApprovalAllow);
                let always = t(Msg::ApprovalAlways);
                let deny = t(Msg::ApprovalDeny);
                let tool_label = if detail.is_empty() {
                    format!("{}: ", tool)
                } else {
                    format!("{}({}): ", tool, detail)
                };
                let prefix_w = crate::width::display_width(&waiting);
                let cont_pad = " ".repeat(prefix_w);

                // Build the chips text — reused for both one-line and
                // two-line layouts.
                let chips_plain = format!("Y {allow} A {always} N {deny}");
                let chips_colored = format!(
                    "{rev}{green} Y {reset}{allow}{rev}{cyan} A {reset}{always}{rev}{red} N {reset}{deny}",
                    rev = SGR_REVERSE,
                    green = SGR_GREEN,
                    cyan = SGR_CYAN,
                    red = SGR_RED,
                    reset = SGR_RESET,
                    allow = allow,
                    always = always,
                    deny = deny,
                );
                let chips_display_w = crate::width::display_width(&chips_plain);
                let screen_w = self.width as usize;

                // Build the label row (with or without color), wrap it,
                // and measure the last wrapped chunk's visible width.
                // This mirrors retained.rs which measures the last
                // build_prefixed_rows row's cell width.
                let label_raw = if self.caps.colors {
                    format!(
                        "{bold}{yellow}{waiting}{tool_label}{reset}",
                        bold = SGR_BOLD,
                        yellow = SGR_YELLOW,
                        reset = SGR_RESET,
                    )
                } else {
                    format!("{waiting}{tool_label}")
                };
                let wrapped_label = wrap_to_width_sgr_aware(&label_raw, screen_w);
                let last_label_w = wrapped_label
                    .last()
                    .map(|s| line_display_width_sgr_aware(s))
                    .unwrap_or(0);

                if last_label_w + chips_display_w <= screen_w {
                    // Everything fits on one line.
                    if self.caps.colors {
                        let row = format!(
                            "{label_raw}{chips}",
                            label_raw = label_raw,
                            chips = chips_colored,
                        );
                        self.push_body_row_raw(row);
                    } else {
                        let row = format!("{label_raw}{chips_plain}");
                        self.push_body_row_raw(row);
                    }
                } else {
                    // Label too long — keep chips on a separate line.
                    self.push_body_row_raw(label_raw);
                    if self.caps.colors {
                        self.push_body_row(format!("{cont_pad}{chips_colored}"));
                    } else {
                        self.push_body_row(format!("{cont_pad}{chips_plain}"));
                    }
                }
            }

            // ── body: command output / errors ──
            UiLine::CommandOutput(text) => {
                self.push_command_output(&text);
            }
            UiLine::ImageAttachment(n) => {
                // `└` at col 2, aligned under the `[` of `[Image #N]`
                // in the user-message echo above (push_user prefixes
                // `❯ ` so user content starts at col 2). alt-screen's
                // push_command_output passes through verbatim — no
                // PAD_COL auto-prefix — so we emit the leading 2
                // spaces explicitly here. Mirrors retained's render
                // visually: same `└` column, same indent under the
                // parent user message.
                //
                // Tight grouping: `push_user` always emits a trailing
                // blank spacer row. Pop it if present so the attachment
                // sits flush under the user message (no orphan blank
                // between `❯ msg` and `└ [Image #N]`), then re-emit a
                // fresh trailing blank so the next turn's content still
                // has paragraph separation.
                if self.body_lines.last().map_or(false, |r| r.is_empty()) {
                    self.body_lines.pop();
                }
                self.push_command_output(&format!("  └ [Image #{}]", n));
                self.push_body_row(String::new());
            }
            UiLine::VisionPreprocessSuccess { msg, model } => {
                // alt-screen has no two-style row primitive; degrade to
                // a plain command-output line concatenating message and
                // model. Loses the gray styling but preserves the
                // information. Acceptable for the alt-screen path
                // (used in non-retained terminals).
                //
                // Trailing blank: paragraph separation before the next
                // event (spinner / assistant text). Mirrors retained.
                self.push_command_output(&format!("{}  {}", msg, model));
                self.push_body_row(String::new());
            }
            UiLine::Error(msg) => {
                self.push_error(&msg);
            }
            UiLine::Warning(msg) => {
                self.push_warning(&msg);
            }

            // ── footer: input box ──
            UiLine::InputPrompt {
                buf,
                cursor_byte,
                menu,
                status,
                attachments,
            } => {
                self.pending_input = Some((buf, cursor_byte));
                self.pending_status = status;
                self.pending_menu = menu; // slash-palette payload
                self.pending_attachments = attachments;
                self.pending_spinner = None; // input takes over from spinner
                self.footer_dirty = true;
                // Menu / attachment state changes the footer height
                // (variable rows). Repaint body too so it shrinks/grows
                // correspondingly.
                self.body_dirty = true;
            }
            UiLine::StreamingBox {
                buf,
                cursor_byte,
                frame,
                label,
                status,
                menu,
                attachments,
            } => {
                self.pending_input = Some((buf, cursor_byte));
                self.pending_status = status;
                self.pending_menu = menu;
                self.pending_attachments = attachments;
                self.pending_spinner = Some((frame, label));
                self.footer_dirty = true;
                self.body_dirty = true;
            }
            UiLine::InputCommit => {
                // The committed buffer became a `User` body row already
                // (event loop emits both); just clear input state so
                // the next paint shows an empty prompt.
                self.pending_input = Some((String::new(), 0));
                self.footer_dirty = true;
            }
            UiLine::Spinner { frame, label } => {
                self.pending_spinner = Some((frame, label));
                self.footer_dirty = true;
            }
            UiLine::ClearTransient => {
                self.pending_spinner = None;
                self.footer_dirty = true;
            }
        }

        // Repaint after every render call. Both paint helpers are
        // no-ops when their *_dirty flag is false, so unconditional
        // calls cost only the branch — far cleaner than threading
        // dirty checks through every match arm.
        self.paint_frame();
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn shutdown(&mut self) {
        self.leave_alt_screen();
    }

    fn reset(&mut self) {
        // Wipe body_lines + viewport state, repaint blank canvas.
        // Used by `/clear` slash command. Footer state preserved so
        // the input box / status keep their value across the wipe.
        self.body_lines.clear();
        self.assistant_line_buf.clear();
        self.viewport_top = 0;
        self.sticky_bottom = true;
        self.body_dirty = true;
        self.footer_dirty = true;
        // Selection indices reference `body_lines`, which we just
        // wiped — keep them around and they'd point past end-of-
        // buffer on the next paint.
        self.selection.clear();
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        self.paint_frame();
    }

    fn clear_screen(&mut self) {
        // Same shape as reset: wipe everything. The slash `/clear`
        // semantic is "remove visible content"; in alt-screen there's
        // no host scrollback to preserve, so wiping body_lines too is
        // consistent with what the user expects ("a clean slate").
        self.reset();
    }

    fn suspend_for_external(&mut self) {
        // To run an external child cleanly we pop alt-screen so the
        // child sees the host terminal's main screen. resume re-enters.
        self.leave_alt_screen();
    }

    fn resume_from_external(&mut self) {
        self.enter_alt_screen();
        // After re-entering, the alt-screen is blank — repaint our
        // entire body buffer + footer chrome.
        self.body_dirty = true;
        self.footer_dirty = true;
        self.paint_frame();
    }

    fn flush_deferred(&mut self) {
        // Phase 5+ adds frame coalescing. For now, nothing buffered.
    }

    fn scroll_body(&mut self, delta: i32) {
        let body_height = self.body_height() as usize;
        let total = self.body_lines.len();
        let max_top = total.saturating_sub(body_height);

        // Compute the new viewport_top. Treat sticky_bottom as
        // viewport_top = max_top so a user scrolling up from the
        // pinned-bottom state lands one page above the tail (not
        // anchored at 0 because the buffer might be much longer than
        // one page).
        let current_top = if self.sticky_bottom { max_top } else { self.viewport_top };
        let new_top: usize = if delta < 0 {
            current_top.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (current_top + delta as usize).min(max_top)
        };

        self.viewport_top = new_top;
        // Sticky-bottom transitions:
        //   * Scrolling up (or anywhere short of max_top) breaks sticky.
        //   * Scrolling down past the end re-pins to bottom — new
        //     content auto-follows again from there.
        self.sticky_bottom = new_top >= max_top;
        self.body_dirty = true;
        // Footer also dirty: paint_body's last cursor position lands
        // somewhere in the body region, but the user expects the
        // terminal cursor to stay in the input row at the right
        // buf-prefix offset. Without this flag, paint_frame would
        // skip paint_footer and leave the cursor stranded mid-body.
        self.footer_dirty = true;
        self.paint_frame();
    }

    fn scroll_body_to_top(&mut self) {
        self.viewport_top = 0;
        self.sticky_bottom = false;
        self.body_dirty = true;
        self.footer_dirty = true;
        self.paint_frame();
    }

    fn scroll_body_to_bottom(&mut self) {
        let body_height = self.body_height() as usize;
        self.viewport_top = self.body_lines.len().saturating_sub(body_height);
        self.sticky_bottom = true;
        self.body_dirty = true;
        self.footer_dirty = true;
        self.paint_frame();
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        // No-op if size unchanged. Pairs with the burst coalescing in
        // `event_loop::handle_input`; same-size events still arrive
        // (focus changes, tab cycles, multiplexer pane shuffles) and
        // the `\x1b[2J\x1b[H` wipe below is visible flicker even when
        // the result is byte-identical.
        if cols == self.width && rows == self.height {
            return;
        }
        // Resize is the simplest of all renderers in alt-screen mode:
        // no DECSTBM region to renegotiate, no scroll-region edge
        // cases, no auto-wrap-into-footer issues. We just:
        //   1. update cached size
        //   2. re-flow body_lines from raw_body_lines at the new width
        //      so narrowing the terminal wraps (not truncates) long
        //      rows and widening re-merges previously split short rows
        //      (issue #363)
        //   3. wipe the alt-screen with `\x1b[2J\x1b[H` so stale
        //      pre-resize glyphs at old absolute positions can't
        //      ghost — iTerm2 / some terminals leave them visible
        //      until something overwrites them
        //   4. mark both panes dirty + repaint
        self.width = cols;
        self.height = rows;
        // Re-flow body content at the new width. raw_body_lines
        // preserves the original (unwrapped) logical lines so that
        // widening the terminal re-merges previously split rows and
        // narrowing the terminal splits long rows instead of
        // truncating them.
        self.reflow_body_lines();
        // Re-clamp viewport_top against the new (possibly smaller)
        // body_height, so a user who'd Page-Up'd into the buffer
        // doesn't end up with viewport_top past end-of-buffer.
        let new_body_height = self.body_height() as usize;
        self.viewport_top = self
            .viewport_top
            .min(self.body_lines.len().saturating_sub(new_body_height));
        // Selection's display-column anchors were taken at the old
        // width; after a resize they'd land in the wrong spot of the
        // re-flowed line. Cleanest is to drop the selection entirely
        // — the user can drag-select again at the new geometry.
        self.selection.clear();
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        self.body_dirty = true;
        self.footer_dirty = true;
        self.paint_frame();
    }

    fn begin_selection(&mut self, col: u16, row: u16) {
        if let Some(pos) = self.screen_to_body(col, row) {
            self.selection.begin(pos);
        } else {
            self.selection.clear();
        }
        self.body_dirty = true;
        self.paint_frame();
    }

    fn update_selection(&mut self, col: u16, row: u16) {
        if let Some(pos) = self.screen_to_body(col, row) {
            self.selection.update(pos);
            self.body_dirty = true;
            self.paint_frame();
        }
    }

    fn end_selection(&mut self) {
        if let Some(text) = self.selection.end(&self.body_lines) {
            // copy_to_clipboard fans out to OSC 52 + arboard so Terminal.app
            // (OSC 52 off by default) and headless terminals both get the
            // selection. Pure OSC 52 would silently drop on those.
            crate::render::selection::copy_to_clipboard(&mut self.out, &text);
        }
    }

    fn copy_selection(&mut self) -> bool {
        let copied = self.selection.copy(&self.body_lines);
        if copied {
            self.body_dirty = true;
            self.paint_frame();
        }
        copied
    }

    fn toggle_scrollbar(&mut self) -> bool {
        self.show_scrollbar = !self.show_scrollbar;
        let mut state = crate::render::ui_state::load();
        state.ui.show_scrollbar = self.show_scrollbar;
        crate::render::ui_state::save(&state);
        // Body width changes — force reflow + repaint
        self.reflow_body_lines();
        self.body_dirty = true;
        self.paint_frame();
        self.show_scrollbar
    }

    fn scroll_to_prev_message(&mut self) {
        let body_height = self.body_height() as usize;
        let max_top = self.body_lines.len().saturating_sub(body_height);
        let effective_top = if self.sticky_bottom { max_top } else { self.viewport_top };
        let target = self.message_marks.iter().rev()
            .find(|m| m.line_idx < effective_top)
            .map(|m| m.line_idx);
        if let Some(t) = target { self.scroll_body_to(t); }
    }

    fn scroll_to_next_message(&mut self) {
        let body_height = self.body_height() as usize;
        let max_top = self.body_lines.len().saturating_sub(body_height);
        let effective_top = if self.sticky_bottom { max_top } else { self.viewport_top };
        let target = self.message_marks.iter()
            .find(|m| m.line_idx > effective_top)
            .map(|m| m.line_idx);
        if let Some(t) = target { self.scroll_body_to(t); }
    }

    fn scroll_to_prev_user_message(&mut self) {
        let body_height = self.body_height() as usize;
        let max_top = self.body_lines.len().saturating_sub(body_height);
        let effective_top = if self.sticky_bottom { max_top } else { self.viewport_top };
        let target = self.message_marks.iter().rev()
            .find(|m| m.line_idx < effective_top && m.kind == crate::render::MarkKind::User)
            .map(|m| m.line_idx);
        if let Some(t) = target { self.scroll_body_to(t); }
    }

    fn scroll_to_next_user_message(&mut self) {
        let body_height = self.body_height() as usize;
        let max_top = self.body_lines.len().saturating_sub(body_height);
        let effective_top = if self.sticky_bottom { max_top } else { self.viewport_top };
        let target = self.message_marks.iter()
            .find(|m| m.line_idx > effective_top && m.kind == crate::render::MarkKind::User)
            .map(|m| m.line_idx);
        if let Some(t) = target { self.scroll_body_to(t); }
    }
}

impl<W: Write + Send> Drop for AltScreenRenderer<W> {
    fn drop(&mut self) {
        // Belt-and-suspenders pop. `shutdown()` already runs on
        // normal exit and `leave_alt_screen` is idempotent (gated
        // on `alt_screen_active`), so the duplicate pop is safe.
        // This Drop is what saves the user's terminal when a panic
        // bypasses `shutdown()`.
        self.leave_alt_screen();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_default() -> TerminalCaps {
        TerminalCaps {
            tty: true,
            colors: true,
            spinner: true,
            bracketed_paste: true,
            raw_mode: true,
            scroll_region: true,
            unicode_symbols: true,
        }
    }

    /// Construction enters alt-screen + enables mouse capture.
    /// Drop reverses both. The lifecycle is what the rest of Phase 1
    /// hangs off — if this is wrong, every later test is moot.
    #[test]
    fn construct_emits_alt_screen_enter_sequence() {
        let mut buf = Vec::new();
        let r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("\x1b[?1049h"), "alt-screen ENTER missing. got: {:?}", s);
        assert!(s.contains("\x1b[?1002h"), "mouse-mode ENTER (1002h) missing. got: {:?}", s);
        assert!(s.contains("\x1b[?1006h"), "mouse-mode ENTER (1006h) missing. got: {:?}", s);
        assert!(s.contains("\x1b[?1049l"), "alt-screen LEAVE missing. got: {:?}", s);
        assert!(s.contains("\x1b[?1002l"), "mouse-mode LEAVE (1002l) missing. got: {:?}", s);
        assert!(s.contains("\x1b[?1006l"), "mouse-mode LEAVE (1006l) missing. got: {:?}", s);
    }

    /// Welcome pushes 4 rows (title, working_dir, model, blank) into
    /// body_lines and paint_body emits each at absolute CUP. Phase 2:
    /// no longer "renders at fixed rows 1/2/3" — rows are derived from
    /// body_lines + viewport, but in a fresh session the welcome lands
    /// at the top of the buffer so rows 1-4 still hold its content.
    #[test]
    fn welcome_pushes_four_body_rows_at_top() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::Welcome {
            model: "claude-opus-4-7".into(),
            working_dir: "/tmp/proj".into(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // First three rows of the body received CUP + content.
        assert!(s.contains("\x1b[1;1H"), "row 1 CUP missing. got: {:?}", s);
        assert!(s.contains("\x1b[2;1H"), "row 2 CUP missing. got: {:?}", s);
        assert!(s.contains("\x1b[3;1H"), "row 3 CUP missing. got: {:?}", s);
        assert!(
            s.contains("AtomCode"),
            "welcome banner must include 'AtomCode'. got: {:?}",
            s
        );
        assert!(
            s.contains("claude-opus-4-7"),
            "welcome banner must include the model name. got: {:?}",
            s
        );
        assert!(
            s.contains("/tmp/proj"),
            "welcome banner must include the working dir. got: {:?}",
            s
        );
    }

    /// Multiline user input (`\<Enter>` on terminals that swallow
    /// Shift/Alt+Enter — typical Windows cmd.exe / legacy conhost,
    /// where the modifier bits never reach the application — plus
    /// pasted content with embedded newlines) MUST split into one
    /// body row per physical line. Was a single body string with
    /// embedded `\n`, which `paint_body` writes verbatim — the
    /// terminal interprets LF as row-advance, and the next CUP+EL
    /// for the following body row erases whatever landed there.
    /// User-reported on Windows cmd: "abc<\><Enter>def" submitted as
    /// echo only showed `❯ abc`, the `def` flashed and disappeared.
    #[test]
    fn push_user_splits_on_newline_into_separate_body_rows() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::User("first\nsecond\nthird".into()));
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("first"), "first line missing. got: {:?}", s);
        assert!(s.contains("second"), "second line missing. got: {:?}", s);
        assert!(s.contains("third"), "third line missing. got: {:?}", s);
        // No raw `\n` survives into a single painted body row —
        // `paint_body` CUPs each row independently, so multi-line
        // echo must emit each line through `push_body_row` separately.
        assert!(
            !s.contains("first\nsecond"),
            "multiline echo must not embed raw \\n in a single body row \
             (would corrupt alt-screen layout). got: {:?}",
            s
        );
    }

    /// Phase 2: User / AssistantText / ToolCall / ToolResult / Error
    /// all push body rows. Verify each surfaces in the painted output.
    #[test]
    fn body_uilines_render_into_viewport() {
        // UiLine::Error localizes via i18n — pin the locale so a
        // concurrent test that flipped to ZhCn doesn't make this
        // assertion see `[错误：boom]`.
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::User("hi".into()));
        r.render(UiLine::AssistantText("hello there\n".into()));
        r.render(UiLine::AssistantLineBreak);
        r.render(UiLine::ToolCall {
            name: "read_file".into(),
            detail: "x.rs".into(),
        });
        r.render(UiLine::ToolResult {
            success: true,
            summary: "ok".into(),
        });
        r.render(UiLine::Error("boom".into()));
        r.render(UiLine::TurnComplete);
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("hi"), "user echo missing. got: {:?}", s);
        assert!(s.contains("hello there"), "assistant text missing. got: {:?}", s);
        assert!(s.contains("read_file"), "tool call name missing. got: {:?}", s);
        assert!(s.contains("ok"), "tool result summary missing. got: {:?}", s);
        assert!(s.contains("[Error: boom]"), "error line missing. got: {:?}", s);
    }

    /// Each body push produces a paint cycle that EL-clears every row
    /// in the viewport (including ones past end-of-content) so a
    /// previous frame's content can't ghost. Phase 3: body_height =
    /// height − footer_rows, so verify the BODY rows specifically (1..=7
    /// when height=10, footer_rows=3) all get CUP+EL.
    #[test]
    fn paint_body_clears_every_viewport_row() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::User("hi".into()));
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // 10-row terminal − 3-row footer = 7-row body. Body paints
        // emit CUP+EL for rows 1..=7.
        for row in 1..=7u16 {
            assert!(
                s.contains(&format!("\x1b[{};1H", row)),
                "row {} CUP missing. got: {:?}",
                row,
                s
            );
        }
    }

    /// Bounded buffer: when body_lines exceeds max_scrollback_rows,
    /// oldest rows drop from the front. Sanity-check via direct field
    /// access (bypass the env var by going through with_writer + manual
    /// max_scrollback_rows override via test-only API). Keep the cap
    /// small so the test runs fast.
    #[test]
    fn bounded_buffer_drops_front_rows_on_overflow() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        // Override the cap directly. Field is private but we're in the
        // same module so this is fine for tests.
        r.max_scrollback_rows = 5;
        for i in 0..10 {
            r.push_body_row(format!("row {}", i));
        }
        // Cap is 5, pushed 10 → only the last 5 should remain (rows 5..9).
        assert_eq!(r.body_lines.len(), 5, "buffer must be capped at 5");
        assert_eq!(r.body_lines[0], "row 5");
        assert_eq!(r.body_lines[4], "row 9");
        drop(r);
    }

    /// sticky_bottom (default) shows the TAIL of body_lines. With more
    /// body rows than viewport height, only the last viewport_height
    /// rows should be in the painted output.
    #[test]
    fn sticky_bottom_shows_tail_when_body_exceeds_viewport() {
        let mut buf = Vec::new();
        // Phase 4.5: footer reserves 5 rows. Use height=10 so body_height=5.
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        for i in 0..10 {
            r.push_body_row(format!("ROW{}", i));
        }
        r.body_dirty = true;
        r.paint_body();
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // 5-row body viewport, 10 body rows → tail = ROW5..ROW9.
        // ROW0..ROW4 must NOT be in the most recent painted output.
        // Since each paint emits all 5 rows, the latest paint contains
        // ROW5..ROW9.
        for i in 5..10 {
            assert!(
                s.contains(&format!("ROW{}", i)),
                "expected ROW{} in tail. got: {:?}",
                i,
                s
            );
        }
        // The leading rows might still appear in EARLIER paints (one
        // per push_body_row when called via render()); we don't assert
        // their absence — only that the tail is present in the final
        // state. This test would need a "rendered final frame only"
        // helper for stronger assertions; out of scope for Phase 2.
    }

    /// Assistant streaming: chunks accumulate in assistant_line_buf
    /// across multiple AssistantText events; complete physical lines
    /// (terminated by `\n`) get pushed into body_lines; trailing
    /// partial chunks stay in the buffer until AssistantLineBreak or
    /// TurnComplete flushes them.
    #[test]
    fn assistant_streaming_buffers_until_newline_or_break() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        // First chunk has no newline — should buffer, not push.
        r.render(UiLine::AssistantText("hello ".into()));
        assert_eq!(r.body_lines.len(), 0, "no newline yet → no body row");
        assert_eq!(r.assistant_line_buf, "hello ");

        // Second chunk completes the line with `\n` → push.
        r.render(UiLine::AssistantText("world\n".into()));
        assert_eq!(r.body_lines.len(), 1, "newline triggers push");
        assert_eq!(r.body_lines[0], "hello world");
        assert!(r.assistant_line_buf.is_empty(), "buffer drained on \\n");

        // Trailing chunk without newline → buffer again.
        r.render(UiLine::AssistantText("tail ".into()));
        assert_eq!(r.body_lines.len(), 1, "trailing chunk doesn't push yet");

        // AssistantLineBreak forces flush.
        r.render(UiLine::AssistantLineBreak);
        assert_eq!(r.body_lines.len(), 2, "AssistantLineBreak flushes");
        assert_eq!(r.body_lines[1], "tail ");
        drop(r);
    }

    /// TurnSeparator pushes 3 rows: blank, ─── label ───, blank.
    /// Mirrors the visual breathing-room used by retained mode.
    #[test]
    fn turn_separator_pushes_three_rows() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::TurnSeparator {
            label: "Done".into(),
        });
        assert_eq!(r.body_lines.len(), 3);
        assert!(r.body_lines[0].is_empty(), "first row is blank spacer");
        assert!(r.body_lines[1].contains("Done"), "middle row has label");
        assert!(r.body_lines[1].contains("─"), "middle row has rule chars");
        assert!(r.body_lines[2].is_empty(), "third row is blank spacer");
        drop(r);
    }

    /// Phase 3.5: assistant text routes through `markdown::render_line`,
    /// so inline markdown syntax (`**bold**`) becomes ANSI SGR (bold
    /// escape) when caps.colors is on. Verify a complete-line streaming
    /// sequence ends with a body row containing the bold SGR sequence.
    #[test]
    fn assistant_text_renders_inline_bold_via_markdown() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::AssistantText("This is **bold** text\n".into()));
        // After the newline the line gets pushed.
        assert_eq!(r.body_lines.len(), 1);
        let row = &r.body_lines[0];
        // Bold SGR is `\x1b[1m` ... `\x1b[22m` (or `\x1b[0m` reset).
        assert!(
            row.contains("\x1b[1m"),
            "bold SGR opener missing — markdown didn't fire. got: {:?}",
            row
        );
        assert!(row.contains("bold"), "literal text retained. got: {:?}", row);
        drop(r);
    }

    /// Phase 3.5: `# Heading` becomes a styled body row (markdown
    /// renderer applies bold + colour for headings). Just verify the
    /// SGR emerges; we don't assert exact escape since the renderer
    /// may evolve heading style.
    #[test]
    fn assistant_heading_renders_with_sgr() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::AssistantText("# My Heading\n".into()));
        assert_eq!(r.body_lines.len(), 1);
        let row = &r.body_lines[0];
        assert!(
            row.contains("\x1b["),
            "heading should have SGR styling. got: {:?}",
            row
        );
        assert!(row.contains("My Heading"));
        drop(r);
    }

    /// Phase 3.5 (updated for buffer-and-flush): a ```fenced``` block now
    /// buffers body lines until close fence. The fence-open line and each
    /// body line return None (no body row pushed). The close fence flushes
    /// the whole block as a single body row containing all lines (with
    /// per-line indent).
    #[test]
    fn fenced_code_block_state_carries_across_streaming_chunks() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);

        r.render(UiLine::AssistantText("```rust\n".into()));
        // Fence-open line doesn't render — md_state.in_code_block flips on,
        // no body row pushed and no body_dirty for an empty fence.
        assert_eq!(r.body_lines.len(), 0, "fence-open line must not push");
        assert!(r.md_state.in_code_block, "code-block state must flip on");

        r.render(UiLine::AssistantText("let x = 1;\n".into()));
        // Buffered — no body row pushed yet, code_buf has 1 entry, state still on.
        assert_eq!(
            r.body_lines.len(),
            0,
            "body line inside open fence must buffer, not push"
        );
        assert_eq!(r.md_state.code_buf.len(), 1, "code_buf must hold the body line");
        assert!(r.md_state.in_code_block);

        r.render(UiLine::AssistantText("```\n".into()));
        // Close fence flushes — state off, code_buf empty, at least one body row
        // pushed containing the flushed (highlighted-or-plain) block.
        assert!(!r.md_state.in_code_block, "code-block state must flip off");
        assert!(r.md_state.code_buf.is_empty(), "code_buf must be drained");
        assert!(r.body_lines.len() >= 1, "close fence must flush at least one body row");
        drop(r);
    }

    /// Phase 3.5: `push_user` resets md_state so a previous turn's
    /// stuck-open fence can't bleed into the new turn.
    #[test]
    fn user_turn_resets_markdown_state() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        // Open a fence in turn 1, never close.
        r.render(UiLine::AssistantText("```\n".into()));
        assert!(r.md_state.in_code_block);

        // New user turn — md_state should reset.
        r.render(UiLine::User("next question".into()));
        assert!(
            !r.md_state.in_code_block,
            "User turn must reset md_state.in_code_block"
        );
        drop(r);
    }

    /// `reset()` (and `clear_screen()` which forwards to reset) wipes
    /// body_lines and the assistant streaming buffer so the next paint
    /// starts from a blank slate.
    #[test]
    fn reset_wipes_body_lines_and_streaming_buffer() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::User("first".into()));
        r.render(UiLine::AssistantText("partial chunk".into()));
        assert!(!r.body_lines.is_empty());
        assert!(!r.assistant_line_buf.is_empty());

        r.reset();
        assert!(r.body_lines.is_empty(), "body_lines wiped on reset");
        assert!(r.assistant_line_buf.is_empty(), "buffer wiped on reset");
        drop(r);
    }

    /// Phase 4.5: footer is now 5 rows (spinner | top_rule | input |
    /// bot_rule | status). With height=10, footer_top=6, so:
    /// spinner@6, top_rule@7, input@8, bot_rule@9, status@10.
    #[test]
    fn input_prompt_renders_at_footer_with_cursor() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::InputPrompt {
            buf: "hello".into(),
            cursor_byte: 5,
            menu: None,
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("\x1b[8;1H"), "input row CUP at row 8 missing. got: {:?}", s);
        assert!(s.contains("hello"), "input buf missing. got: {:?}", s);
        // Cursor at row 8 col 8 (chevron 2 cols + 5 buf chars + 1 for
        // 1-indexed) followed by show-cursor.
        assert!(
            s.contains("\x1b[8;8H\x1b[?25h"),
            "cursor must be positioned at end of buf with show-cursor. got: {:?}",
            s
        );
    }

    /// Phase 4.5: status bar at row 10 (height=10, last row).
    #[test]
    fn status_bar_renders_model_and_cwd() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine {
                model: "claude-opus-4-7".into(),
                cwd: "/tmp/proj".into(),
                ..Default::default()
            },
            attachments: Vec::new(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("\x1b[10;1H"), "status row CUP at row 10 missing. got: {:?}", s);
        assert!(
            s.contains("claude-opus-4-7 \u{00b7} /tmp/proj"),
            "status content missing. got: {:?}",
            s
        );
        assert!(s.contains("\x1b[2m"), "status should be dim. got: {:?}", s);
    }

    /// Phase 4.5: top + bottom rules render as cyan ━ across full width.
    /// (Heavy variant ━ U+2501 instead of light ─ U+2500 — see
    /// `paint_footer` for the legacy-conhost dashed-look rationale.)
    #[test]
    fn input_box_has_top_and_bottom_rules() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 20, 10);
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // top_rule at row 7, bot_rule at row 9. Each row has 20 ━.
        let twenty_heavy = "━".repeat(20);
        assert!(s.contains("\x1b[7;1H"), "top rule row CUP missing. got: {:?}", s);
        assert!(s.contains("\x1b[9;1H"), "bot rule row CUP missing. got: {:?}", s);
        assert!(
            s.contains(&twenty_heavy),
            "20 ━ chars missing. got: {:?}",
            s
        );
        // Bright cyan (96) — matches retained's `Palette::BORDER`.
        assert!(s.contains("\x1b[96m"), "rule should be bright cyan. got: {:?}", s);
    }

    /// `wrap_to_width_sgr_aware` is the soft-wrap helper that keeps long
    /// CommandOutput lines (notably the `/login` OAuth URL) selectable
    /// in alt-screen mode. Direct tests on the helper since it owns the
    /// CSI / Unicode-width edge cases.
    #[test]
    fn wrap_to_width_sgr_aware_handles_url_and_csi_and_wide_chars() {
        // Empty input still produces one (empty) chunk so callers
        // preserve the blank-line invariant.
        assert_eq!(wrap_to_width_sgr_aware("", 10), vec![String::new()]);

        // Short line under width → single chunk, untouched.
        assert_eq!(
            wrap_to_width_sgr_aware("hello", 10),
            vec!["hello".to_string()]
        );

        // Realistic OAuth URL ≈ 200 chars on an 80-col terminal: must
        // produce ≥ 3 chunks, every chunk ≤ 80 display cols, and the
        // concatenation must reproduce the input byte-for-byte.
        let url = "https://atomgit.com/oauth/authorize?client_id=85a8b0099b4144a19a7542d5cc90fdcc&redirect_uri=https%3A%2F%2Facs.atomgit.com%2Fcallback&response_type=code&state=atomcode_1777469916784730326_e2d348c6072a47beb1b0b414f25c8ef6&scope=user_info+projects";
        let chunks = wrap_to_width_sgr_aware(url, 80);
        assert!(chunks.len() >= 3, "URL must wrap into ≥3 chunks, got {}", chunks.len());
        for c in &chunks {
            assert!(
                line_display_width_sgr_aware(c) <= 80,
                "chunk exceeds width: {:?}",
                c
            );
        }
        assert_eq!(chunks.join(""), url, "wrapped chunks must round-trip");

        // CSI sequences contribute zero width and stay attached to
        // their current chunk (no spurious wraps mid-escape).
        let with_sgr = format!("\x1b[31m{}\x1b[0m", "x".repeat(10));
        let chunks = wrap_to_width_sgr_aware(&with_sgr, 5);
        assert_eq!(chunks.len(), 2, "10 visible chars at width 5 → 2 chunks");
        assert!(chunks[0].contains("\x1b[31m"), "opening SGR stays in first chunk");
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), with_sgr.len());

        // Wide CJK glyph (2 cells) at the boundary wraps cleanly
        // instead of being split across chunks.
        let cjk = "ab中文de"; // widths: 1 1 2 2 1 1 = 8
        let chunks = wrap_to_width_sgr_aware(cjk, 3);
        for c in &chunks {
            assert!(line_display_width_sgr_aware(c) <= 3);
        }
        assert_eq!(chunks.join(""), cjk);
    }

    /// Long `CommandOutput` (e.g. the OAuth URL) must end up as multiple
    /// body rows so the entire content is visible AND selectable in
    /// alt-screen mode. Regression: previously a 200-char URL became
    /// one body row that `paint_body` truncated at the right edge,
    /// making the tail uncopyable.
    #[test]
    fn command_output_wraps_long_url_into_multiple_body_rows() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        let url = "https://atomgit.com/oauth/authorize?client_id=85a8b0099b4144a19a7542d5cc90fdcc&redirect_uri=https%3A%2F%2Facs.atomgit.com%2Fcallback&response_type=code&state=atomcode_1777469916784730326_e2d348c6072a47beb1b0b414f25c8ef6&scope=user_info+projects";
        let body = format!("  Open this URL in any browser to sign in to AtomGit:\n  {}\n", url);
        r.render(UiLine::CommandOutput(body));
        r.flush();
        // Header line + ≥3 wrapped URL rows + trailing blank.
        assert!(
            r.body_lines.len() >= 4,
            "long URL must wrap into ≥4 body rows, got {}: {:#?}",
            r.body_lines.len(),
            r.body_lines
        );
        for line in &r.body_lines {
            assert!(
                line_display_width_sgr_aware(line) <= 80,
                "body row exceeds 80 cols: {:?}",
                line
            );
        }
        // Every byte of the URL must survive somewhere in body_lines so
        // the user can still select-and-copy the whole thing.
        let joined: String = r.body_lines.iter().cloned().collect::<Vec<_>>().join("");
        assert!(
            joined.contains(url),
            "wrapped body rows must reconstruct the full URL"
        );
        drop(r);
    }

    /// Phase 4.5: slash menu palette grows the footer dynamically.
    /// 4 menu items → footer_rows = 5 + 4 = 9. body_height shrinks.
    #[test]
    fn slash_menu_grows_footer_and_shrinks_body() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        let baseline_body = r.body_height();
        assert_eq!(baseline_body, 24 - 5, "no menu → body = 24 - 5 = 19");

        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(crate::render::MenuPayload {
                items: vec![
                    ("login".into(), "sign in".into()),
                    ("model".into(), "switch model".into()),
                    ("exit".into(), "leave".into()),
                ],
                selected: 0,
                    kind: crate::render::MenuKind::SlashCommand,
            }),
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        // 3 menu items → footer = 5 + 3 = 8 → body = 24 - 8 = 16.
        assert_eq!(r.body_height(), 24 - 8);
        drop(r);
    }

    /// Phase 4.5: selected menu item gets reverse-video SGR (`\x1b[7m`)
    /// so keyboard focus is highly visible. Non-selected items get dim.
    #[test]
    fn slash_menu_selected_uses_reverse_video() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(crate::render::MenuPayload {
                items: vec![
                    ("login".into(), "sign in".into()),
                    ("exit".into(), "leave".into()),
                ],
                selected: 1,
                    kind: crate::render::MenuKind::SlashCommand,
            }),
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("\x1b[7m"),
            "selected menu row should use reverse video. got: {:?}",
            s
        );
        // Both items present.
        assert!(s.contains("login"));
        assert!(s.contains("exit"));
    }

    /// Long CJK descriptions (plugin skill listings can have 100+
    /// display columns of Chinese) used to overflow past terminal
    /// width and auto-wrap onto subsequent rows. The next iteration's
    /// CUP+EL only wiped the immediately-next row, so 2+ row wraps
    /// leaked stale glyphs into column 1+ of later menu items.
    /// Truncating each menu body to terminal width keeps everything
    /// confined to a single row per item.
    #[test]
    fn slash_menu_truncates_overlong_body_to_terminal_width() {
        let mut buf = Vec::new();
        // Narrow window to make overflow easy to construct without huge
        // descriptions: 30 cols total.
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 30, 24);
        // First item's description is 60+ display cols of CJK, ~2× wider
        // than the window. Pre-fix this would wrap onto the second
        // item's row. Post-fix: clamped at 30 cols, no wrap.
        let very_long_cjk = "中文描述非常非常长".repeat(5); // 9 chars * 5 = 45 chars * 2 cols = 90 cols
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(crate::render::MenuPayload {
                items: vec![
                    ("first".into(), very_long_cjk.clone()),
                    ("second".into(), "short".into()),
                ],
                selected: 0,
                    kind: crate::render::MenuKind::SlashCommand,
            }),
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        r.flush();
        // Assert each menu row's writeable payload between CUPs fits
        // inside the 30-col window. We can't easily measure visible
        // columns from raw bytes here, but we can assert truncation
        // happened by checking the second item's name is still emitted
        // (it would be drowned by an unbounded first-row wrap).
        let body_lines = r.body_lines.clone();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("first"),
            "first item must be present in output. got: {:?}",
            s
        );
        assert!(
            s.contains("second"),
            "second item must remain visible despite first row's overlong CJK. got: {:?}",
            s
        );
        // The full 90-col CJK description must NOT all be present
        // verbatim — it would only fit if the truncation was bypassed.
        assert!(
            !s.contains(very_long_cjk.as_str()),
            "full overlong CJK description must be truncated, but emit kept the entire run. got: {:?}",
            s
        );
        let _ = body_lines;
    }

    /// Phase 4.5: welcome banner now includes the version (right-aligned)
    /// and the onboarding hints (`type something...`, `/provider...`).
    #[test]
    fn welcome_includes_version_and_hint_lines() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::Welcome {
            model: "claude-opus-4-7".into(),
            working_dir: "/tmp/proj".into(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("AtomCode"));
        assert!(s.contains("MIT"), "license MIT missing from banner. got: {:?}", s);
        assert!(s.contains("type something"), "hint A missing. got: {:?}", s);
        assert!(s.contains("/provider"), "hint B missing. got: {:?}", s);
    }

    /// Phase 3: Spinner sets the spinner-row content; ClearTransient
    /// wipes it. Spinner row is footer-top (row N-2 for footer_rows=3).
    #[test]
    fn spinner_renders_at_footer_top() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::Spinner {
            frame: "\u{280b}",
            label: "Thinking".into(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // Spinner row CUP at row 8 + label.
        assert!(s.contains("\x1b[8;1H"), "spinner row CUP missing. got: {:?}", s);
        assert!(s.contains("Thinking"), "spinner label missing. got: {:?}", s);
    }

    /// The spinner FRAME (the rotating glyph) must be coloured brand
    /// magenta (`\x1b[95m`) when caps.colors is on — visual anchor for
    /// the rotation. Label is bold default-fg (mirrors retained's
    /// `style_bold(Role::Secondary)` in build_spinner_body_row); the
    /// previous SGR_DIM choice rendered as hard-to-read mid-gray on
    /// Windows legacy conhost.
    #[test]
    fn spinner_frame_uses_brand_magenta() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::Spinner {
            frame: "\u{280b}",
            label: "Thinking".into(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("\x1b[95m\u{280b}\x1b[0m"),
            "spinner frame must be wrapped in magenta SGR. got: {:?}",
            s
        );
        // Label is bold + default-fg — bold SGR (\x1b[1m) wraps the
        // label, no foreground colour change. Co-exists with the
        // magenta frame SGR on the same row.
        assert!(
            s.contains("\x1b[1m"),
            "label should be wrapped in bold SGR. got: {:?}",
            s
        );
        assert!(
            !s.contains("\x1b[2m"),
            "label must not use dim SGR (broken on Windows conhost). got: {:?}",
            s
        );
    }

    /// `ClearTransient` flips `pending_spinner` back to None so the
    /// next paint of the spinner row emits only EL (no content).
    /// Verify by inspecting field state directly — checking the byte
    /// stream in the cumulative buffer is fragile because the spinner
    /// row gets repainted multiple times.
    #[test]
    fn clear_transient_drops_pending_spinner() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::Spinner {
            frame: "\u{280b}",
            label: "Thinking".into(),
        });
        assert!(r.pending_spinner.is_some(), "spinner should be active");
        r.render(UiLine::ClearTransient);
        assert!(r.pending_spinner.is_none(), "ClearTransient must drop spinner");
        drop(r);
    }

    /// Plan-mode badge gets brand-color SGR (magenta, mirrors retained
    /// renderer's `Role::Brand`) and is emitted BEFORE the dim
    /// `model · cwd` body so the user sees the mode at a glance. Same
    /// layout as the retained `build_status_row` test, just at the
    /// alt-screen byte-stream level since alt-screen writes raw to
    /// stdout instead of going through the cell-diff renderer.
    #[test]
    fn paint_footer_renders_plan_badge_in_brand_color() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine {
                model: "glm-5".into(),
                cwd: "~/proj".into(),
                ctx_used: 0,
                ctx_window: 0,
                hint: None,
                mode_indicator: Some("PLAN".into()),
                session_name: None,
            },
            attachments: Vec::new(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("\x1b[95m"),
            "PLAN badge must use SGR_MAGENTA (Role::Brand). got: {:?}",
            s
        );
        assert!(
            s.contains("PLAN"),
            "PLAN literal must appear in the rendered status. got: {:?}",
            s
        );
        // Badge precedes the dim model/cwd run — confirm the magenta SGR
        // appears earlier in the byte stream than the dim SGR (\x1b[2m).
        let badge_pos = s
            .find("\x1b[95m")
            .expect("magenta SGR must be present");
        let dim_pos = s
            .find("\x1b[2m")
            .expect("dim SGR (status body) must be present");
        assert!(
            badge_pos < dim_pos,
            "PLAN badge SGR ({}) must precede status-body dim SGR ({}). buf: {:?}",
            badge_pos,
            dim_pos,
            s
        );
    }

    /// After the user runs `/rename`, the conversation name should
    /// appear as a right-aligned cyan-bg pill overlaid on the top
    /// rule of the input box. Mirrors CC's per-conversation badge so
    /// users can confirm which session they're typing into without
    /// running `/status`.
    #[test]
    fn paint_footer_renders_session_name_badge_in_reverse_cyan() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine {
                model: "glm-5".into(),
                cwd: "~/proj".into(),
                ctx_used: 0,
                ctx_window: 0,
                hint: None,
                mode_indicator: None,
                session_name: Some("atomcode加解密".into()),
            },
            attachments: Vec::new(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("atomcode加解密"),
            "session name literal must appear in the rendered footer. got: {:?}",
            s
        );
        // Pill style: SGR_REVERSE (7m) combined with SGR_CYAN (96m) to
        // paint a cyan-filled chip. The exact concatenation order isn't
        // load-bearing — only that both attributes are emitted somewhere
        // in the same byte stream.
        assert!(
            s.contains("\x1b[7m") || s.contains("\x1b[7;"),
            "session-name pill must emit reverse-video SGR (7m). got: {:?}",
            s
        );
        assert!(
            s.contains("\x1b[96m") || s.contains(";96m"),
            "session-name pill must emit cyan SGR (96m). got: {:?}",
            s
        );
    }

    /// When `session_name = None` (auto-named / fresh session) the
    /// footer must NOT emit the reverse-cyan pill on the top rule —
    /// guards against the badge leaking onto sessions the user hasn't
    /// explicitly renamed. We check for the exact `\x1b[7;96m` /
    /// `\x1b[96;7m` SGR combo rather than a bare `\x1b[7;` prefix
    /// because the buffer is full of CUP escapes like `\x1b[7;1H`
    /// (cursor to row 7) which share that prefix but aren't SGR.
    #[test]
    fn paint_footer_no_session_name_emits_no_pill() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine {
                model: "glm-5".into(),
                cwd: "~/proj".into(),
                ctx_used: 0,
                ctx_window: 0,
                hint: None,
                mode_indicator: None,
                session_name: None,
            },
            attachments: Vec::new(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            !s.contains("\x1b[7;96m") && !s.contains("\x1b[96;7m") && !s.contains("\x1b[7m"),
            "no session_name must produce no reverse-cyan pill SGR. got: {:?}",
            s
        );
    }

    /// A name wider than the available budget gets ellipsised — the
    /// alternative is either overflowing the terminal width (visible
    /// wrap glitch) or swallowing the entire top rule (badge eats the
    /// border, the input box loses its visual anchor). Budget leaves
    /// at least 8 cells of `━` rule to the left so the box still
    /// reads as bordered.
    #[test]
    fn paint_footer_truncates_overlong_session_name() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 40, 24);
        let very_long = "这是一个非常非常非常长的会话名字应该被截断省略";
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine {
                model: "glm-5".into(),
                cwd: "~/proj".into(),
                ctx_used: 0,
                ctx_window: 0,
                hint: None,
                mode_indicator: None,
                session_name: Some(very_long.into()),
            },
            attachments: Vec::new(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains('…'),
            "overlong session name must be truncated with ellipsis. got: {:?}",
            s
        );
        assert!(
            !s.contains(very_long),
            "full overlong name must NOT appear verbatim (it'd overflow the rule). got: {:?}",
            s
        );
    }

    /// Default Build mode (`mode_indicator = None`) emits no PLAN
    /// literal — protects against accidental "PLAN" leak when the
    /// status line is rendered for a non-plan session. Mirrors the
    /// retained-renderer guard test.
    #[test]
    fn paint_footer_default_mode_emits_no_plan_badge() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine {
                model: "glm-5".into(),
                cwd: "~/proj".into(),
                ctx_used: 0,
                ctx_window: 0,
                hint: None,
                mode_indicator: None,
                session_name: None,
            },
            attachments: Vec::new(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            !s.contains("PLAN"),
            "no mode_indicator must produce no PLAN literal. got: {:?}",
            s
        );
        // Sanity: model/cwd still present so we know the status row
        // actually rendered (not skipped via some empty-status path).
        assert!(s.contains("glm-5"));
        assert!(s.contains("~/proj"));
    }

    /// `on_resize` is a no-op when the size hasn't actually changed.
    /// Some terminals fire spurious Resize events on focus / tab /
    /// pane-shuffle (no grid change), and the `\x1b[2J\x1b[H` wipe
    /// inside the resize handler is visible flicker even when the
    /// outcome would be byte-identical. Pairs with the burst-coalesce
    /// in `event_loop::handle_input`. Linux Mint / gnome-terminal
    /// users reported "拉伸窗口刷屏" for exactly this reason.
    #[test]
    fn on_resize_same_size_emits_nothing() {
        // Drive two AltScreenRenderer instances against separate
        // capture buffers — one runs a same-size on_resize, the other
        // runs a real resize. Compare their output. (Single-renderer
        // pattern doesn't work because `with_writer` keeps the &mut
        // Vec borrow alive for the renderer's lifetime.)
        let mut baseline = Vec::new();
        {
            let mut r = AltScreenRenderer::with_writer(&mut baseline, caps_default(), 80, 24);
            r.render(UiLine::User("hi".into()));
            r.flush();
            r.on_resize(80, 24); // same size — should be a no-op
            drop(r);
        }

        let mut real_resize = Vec::new();
        {
            let mut r = AltScreenRenderer::with_writer(&mut real_resize, caps_default(), 80, 24);
            r.render(UiLine::User("hi".into()));
            r.flush();
            r.on_resize(60, 16); // different size — should emit wipe + repaint
            drop(r);
        }

        let baseline_str = String::from_utf8_lossy(&baseline);
        let real_str = String::from_utf8_lossy(&real_resize);
        assert!(
            !baseline_str.contains("\x1b[2J\x1b[H"),
            "same-size on_resize must not emit \\x1b[2J\\x1b[H wipe (flicker source). \
             baseline: {:?}",
            baseline_str
        );
        assert!(
            real_str.contains("\x1b[2J\x1b[H"),
            "real resize MUST still emit \\x1b[2J\\x1b[H wipe; got: {:?}",
            real_str
        );
    }

    /// Phase 4: `on_resize` updates cached dimensions, wipes the
    /// alt-screen, and repaints. body_lines are kept verbatim — paint
    /// truncates each row to the new width on the fly so we don't have
    /// to re-flow at resize time.
    #[test]
    fn on_resize_updates_dimensions_and_repaints() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::User("hi".into()));
        assert_eq!(r.width, 80);
        assert_eq!(r.height, 24);

        r.on_resize(60, 16);
        assert_eq!(r.width, 60);
        assert_eq!(r.height, 16);
        // body_height = 16 - 5 = 11.
        assert_eq!(r.body_height(), 11);

        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("\x1b[2J\x1b[H"),
            "on_resize should wipe screen. got: {:?}",
            s
        );
    }

    /// Phase 4: long body lines get clipped to terminal width at paint
    /// time so they don't autowrap into the next row's slot. `truncate_to_width`
    /// is SGR-aware (skips ESC chars in width count) so colour styling
    /// survives the clip.
    #[test]
    fn paint_body_clips_long_lines_to_width() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 20, 10);
        // Push a row much longer than terminal width — 50 chars.
        let long = "a".repeat(50);
        r.push_body_row(long);
        r.body_dirty = true;
        r.paint_body();
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // The terminal is 20 cols wide. After paint, the line should
        // appear at most 20 a's in a single row (no autowrap into
        // the next row).
        let twenty_a = "a".repeat(20);
        assert!(
            s.contains(&twenty_a),
            "20 a's should appear (the visible portion). got: {:?}",
            s
        );
        // 21 a's must NOT appear consecutively — that would mean we
        // failed to truncate and the terminal autowrapped.
        let twenty_one_a = "a".repeat(21);
        assert!(
            !s.contains(&twenty_one_a),
            "long line should be truncated to 20 cols. got: {:?}",
            s
        );
    }

    /// Phase 4: paint emits SGR reset after every row so an open
    /// colour span on one row can't leak into the next row's CUP+EL
    /// region. Verify the reset sequence appears in the output.
    #[test]
    fn paint_body_appends_sgr_reset_per_row() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::User("hi".into()));
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("\x1b[0m"),
            "expected SGR reset after at least one body row. got: {:?}",
            s
        );
    }

    /// scroll_body with negative delta scrolls UP (towards older
    /// content), breaks sticky_bottom, and the next paint shows
    /// earlier rows.
    #[test]
    fn scroll_body_up_breaks_sticky_and_shows_older_rows() {
        let mut buf = Vec::new();
        // height=10 → body_height=5 (Phase 4.5: footer is 5 rows).
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        for i in 0..20 {
            r.push_body_row(format!("R{:02}", i));
        }
        assert!(r.sticky_bottom);
        r.scroll_body(-5);
        assert!(!r.sticky_bottom, "scroll up must break sticky_bottom");
        // viewport_top: max_top = 20 - 5 = 15, after -5 → 10.
        assert_eq!(r.viewport_top, 10);
        drop(r);
    }

    /// scroll_body that lands at max_top (or past) re-pins sticky.
    /// Verifies the auto-follow-on-scroll-down behaviour.
    #[test]
    fn scroll_body_down_to_end_re_pins_sticky() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        for i in 0..20 {
            r.push_body_row(format!("R{:02}", i));
        }
        r.scroll_body(-5); // up first
        assert!(!r.sticky_bottom);
        // Scroll down enough to pass max_top (5 was distance up, scroll
        // down 10 should overshoot and clamp).
        r.scroll_body(10);
        assert!(r.sticky_bottom, "reaching max_top must re-stick to bottom");
        drop(r);
    }

    /// scroll_body_to_top jumps viewport_top to 0 and clears sticky.
    #[test]
    fn scroll_body_to_top_jumps_to_zero() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        for i in 0..20 {
            r.push_body_row(format!("R{:02}", i));
        }
        r.scroll_body_to_top();
        assert_eq!(r.viewport_top, 0);
        assert!(!r.sticky_bottom);
        drop(r);
    }

    /// scroll_body_to_bottom jumps to max_top and re-pins sticky.
    #[test]
    fn scroll_body_to_bottom_jumps_to_max_top_and_sticks() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        for i in 0..20 {
            r.push_body_row(format!("R{:02}", i));
        }
        r.scroll_body_to_top();
        r.scroll_body_to_bottom();
        // body_height = 5, total = 20, max_top = 15.
        assert_eq!(r.viewport_top, 15);
        assert!(r.sticky_bottom);
        drop(r);
    }

    /// While scrolled up, new body content arrives via push_body_row.
    /// sticky_bottom is false → viewport_top stays put → user keeps
    /// looking at old content. body_dirty flips so next paint reflects
    /// the new buffer length but visible content is the same. (When
    /// new content pushes the user's snapshot out of the bounded buffer
    /// front, viewport_top would shift; that's the bounded-buffer test.)
    #[test]
    fn new_content_during_scroll_holds_user_position() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        for i in 0..20 {
            r.push_body_row(format!("R{:02}", i));
        }
        r.scroll_body(-5);
        let pinned_top = r.viewport_top;
        // Append new content while user is scrolled up.
        r.push_body_row("NEW".into());
        // viewport_top unchanged because sticky_bottom was false.
        assert_eq!(r.viewport_top, pinned_top);
        assert!(!r.sticky_bottom);
        drop(r);
    }

    /// Phase 4 edge case: resize that puts viewport_top past the new
    /// end-of-buffer must clamp viewport_top instead of leaving it
    /// in an out-of-range state.
    #[test]
    fn on_resize_clamps_viewport_top_when_buffer_shorter_than_viewport() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        // Push 5 rows; resize to a height that gives body_height=10.
        // viewport_top should clamp to body_lines.len() - body_height,
        // saturating to 0 because 5 < 10.
        for i in 0..5 {
            r.push_body_row(format!("r{}", i));
        }
        r.viewport_top = 3; // simulate user scrolled up
        r.on_resize(80, 13); // body_height = 13 - 3 = 10
        assert_eq!(
            r.viewport_top, 0,
            "viewport_top must clamp to 0 when body_lines.len() < body_height"
        );
        drop(r);
    }

    /// `with_writer` takes terminal width/height; `body_height()`
    /// subtracts footer_rows. Verify the math + saturating-min.
    #[test]
    fn body_height_subtracts_footer_rows_with_min_one() {
        let mut buf = Vec::new();
        let r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        // height=10, footer base = 5 (no menu) → body_height=5.
        assert_eq!(r.body_height(), 5);
        drop(r);

        // Tiny terminal: height=2, footer would consume all → degrade
        // to body_height=1 (saturating min) instead of 0 / underflow.
        let mut buf = Vec::new();
        let r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 2);
        assert_eq!(r.body_height(), 1);
        drop(r);
    }

    /// `suspend_for_external` pops alt-screen so a child process
    /// sees the host terminal's main screen; `resume` re-enters.
    /// Used by the OAuth login flow and any future shell-out.
    #[test]
    fn suspend_resume_pops_and_re_enters_alt_screen() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.suspend_for_external();
        r.resume_from_external();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // Sequence on the wire should be: enter, leave (suspend),
        // enter again (resume), leave (drop). Two of each.
        assert_eq!(
            s.matches("\x1b[?1049h").count(),
            2,
            "expected two ENTERs (construct + resume). got: {:?}",
            s
        );
        assert_eq!(
            s.matches("\x1b[?1049l").count(),
            2,
            "expected two LEAVEs (suspend + drop). got: {:?}",
            s
        );
    }

    /// Regression: when scrollback navigation runs (PageUp / Shift+Up /
    /// mouse wheel) the body region repaints but the terminal cursor
    /// must stay in the input row at the right buf-prefix offset.
    /// Earlier `scroll_body` only flipped `body_dirty`, leaving
    /// `footer_dirty=false` and skipping the input-row CUP at the
    /// end of `paint_footer` — symptom: cursor stranded mid-body
    /// at the last paint_body row, where the user's next keystroke
    /// would visually echo into the conversation history rather than
    /// the input box. Both flags now get set.
    #[test]
    fn scroll_repositions_terminal_cursor_into_input_row() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        // Set up an active InputPrompt so paint_footer has cursor data.
        r.render(UiLine::InputPrompt {
            buf: "hello".into(),
            cursor_byte: 5,
            menu: None,
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        // Push enough body to give scrollback room.
        for i in 0..20 {
            r.push_body_row(format!("R{:02}", i));
        }
        // Scroll then drop so we can read `buf` cleanly. The post-scroll
        // bytes include both the scroll repaint AND the alt-screen pop
        // sequence; we assert on the cursor CUP being present anywhere
        // in those bytes — paint_body alone never emits `\x1b[8;...H`
        // followed by show-cursor (only paint_footer does).
        r.scroll_body(-3);
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // Input row is at row 8 (height 10 - footer 5 + 3 = row 8).
        // After scroll, paint_footer must emit a CUP back to row 8
        // (the input row) followed by show-cursor — otherwise the
        // terminal cursor stays in the last body row. We assert at
        // least one `\x1b[8;{col}H\x1b[?25h` sequence is in the
        // post-scroll bytes.
        assert!(
            s.contains("\x1b[8;") && s.contains("H\x1b[?25h"),
            "scroll must re-emit the input-row cursor CUP. got: {:?}",
            s
        );
    }

    /// Regression: on slow-paint terminals (JediTerm, legacy conhost),
    /// every paint_frame must start by hiding the cursor so its
    /// journey through ~10+ intermediate CUP positions (one per body
    /// row, one per footer row) isn't visible to the user. paint_footer
    /// re-emits show-cursor at its tail when pending_input is set, so
    /// the cursor only appears once at its final position. Reported in
    /// Android Studio's terminal as "cursor jumps around when scrolling
    /// history".
    #[test]
    fn paint_frame_hides_cursor_before_painting_on_slow_terminal() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.slow_paint_terminal = true;
        // Force a paint via any body push.
        r.render(UiLine::User("hello".into()));
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // Hide-cursor (`\x1b[?25l`) must precede the body row CUP
        // sequences — proves we hide before painting, not after.
        let hide_pos = s.find("\x1b[?25l").expect("hide-cursor sequence missing");
        let first_body_cup = s.find("\x1b[1;1H\x1b[K")
            .expect("body row 1 CUP+EL missing");
        assert!(
            hide_pos < first_body_cup,
            "hide-cursor must come before the first body CUP. hide@{}, body@{}, output: {:?}",
            hide_pos,
            first_body_cup,
            s
        );
    }

    /// Regression: on fast terminals (default), paint_frame must NOT
    /// emit `?25l` before paint_body — at streaming framerate the
    /// per-frame `?25l` / `?25h` toggle reads as constant cursor
    /// flicker on macOS Terminal.app even with hardware blink
    /// disabled (`?12l`). Painting body without hiding is safe on
    /// fast terminals because the per-row CUPs flash the cursor
    /// through cells in well under one refresh interval.
    #[test]
    fn paint_frame_does_not_hide_cursor_on_fast_terminal() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        // slow_paint_terminal stays false (default).
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        // Trigger a streaming-style repaint by pushing more body.
        r.render(UiLine::User("hello".into()));
        r.render(UiLine::User("world".into()));
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // No `?25l` before the first body CUP. Drop's leave_alt_screen
        // emits `?25h\x1b[?12h…?1049l` at the end, which contains
        // `?25h` but no `?25l`, so the only way `?25l` could be in the
        // output is from paint_frame — which we don't want.
        if let Some(first_body_cup) = s.find("\x1b[1;1H\x1b[K") {
            let pre = &s[..first_body_cup];
            assert!(
                !pre.contains("\x1b[?25l"),
                "fast terminal must not hide cursor before body paint. output: {:?}",
                s
            );
        }
    }

    /// Regression: on fast terminals, the `?25h` show-cursor sequence
    /// must be emitted at most once for repeated input-prompt frames
    /// — re-emitting it every frame restarts the host terminal's
    /// hardware cursor blink animation, producing visible flicker on
    /// macOS Terminal.app. Subsequent frames must reposition via a
    /// bare CUP only.
    #[test]
    fn fast_terminal_dedupes_show_cursor_across_frames() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        // slow_paint_terminal stays false (default).
        for i in 0..5 {
            r.render(UiLine::InputPrompt {
                buf: format!("typed{}", i),
                cursor_byte: 6,
                menu: None,
                status: crate::render::StatusLine::default(),
                attachments: Vec::new(),
            });
        }
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // Drop's leave_alt_screen emits one `?25h`. paint_footer
        // emits at most one more (on the first frame, transitioning
        // from initial cursor_shown=true → still true via no-op,
        // actually never re-emits because cursor_shown starts true).
        // So the count should be exactly 1 (from leave). If paint_footer
        // were re-emitting per frame we'd see 6+.
        let show_count = s.matches("\x1b[?25h").count();
        assert!(
            show_count <= 1,
            "fast terminal must dedupe show-cursor; got {} occurrences. output: {:?}",
            show_count,
            s
        );
    }

    /// Mouse scroll wheel routes through `scroll_body`. Negative
    /// delta scrolls UP (older content), positive scrolls DOWN.
    /// Verifies the same field-level outcome as keyboard PageUp.
    #[test]
    fn mouse_scroll_via_scroll_body_updates_viewport() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        for i in 0..20 {
            r.push_body_row(format!("R{:02}", i));
        }
        assert!(r.sticky_bottom);
        // Reader emits MouseScroll(-3) for ScrollUp; event_loop calls
        // renderer.scroll_body(-3). Verify here at the renderer level.
        r.scroll_body(-3);
        assert!(!r.sticky_bottom, "scroll up via mouse must break sticky");
        // body_height = 5 (height 10 - footer 5), max_top = 15. -3
        // from sticky-bottom origin → 12.
        assert_eq!(r.viewport_top, 12);
        drop(r);
    }

    // ── selection / clipboard ──
    // Note: pure-helper unit tests (line_display_width_skips_sgr,
    // extract_line_selection_strips_sgr_and_clips_to_range, etc.) live
    // in render::selection::tests — they test functions that now belong
    // to the shared selection module.

    /// Begin → drag → end emits OSC 52 with the selected text.
    ///
    /// `UiLine::User` pushes a body row prefixed with the 2-col
    /// chevron `❯ `, so the visible cols of "hello there" are:
    /// `❯=0 space=1 h=2 e=3 l=4 l=5 o=6 ' '=7 t=8 …`. Drag cols
    /// 2..=6 captures "hello".
    #[test]
    fn drag_select_writes_osc52_to_writer() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::User("hello there".into()));
        r.begin_selection(2, 0);
        r.update_selection(6, 0);
        r.end_selection();
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        let expected = format!(
            "\x1b]52;c;{}\x07",
            crate::render::selection::base64_encode(b"hello")
        );
        assert!(
            s.contains(&expected),
            "OSC 52 with base64('hello') missing. got: {:?}",
            s
        );
    }

    /// Drag end with empty selection (begin only, no movement, head
    /// landed past EOL) writes nothing. We don't want a release that
    /// captured zero chars to clobber the user's existing clipboard.
    #[test]
    fn drag_end_does_not_emit_osc52_when_selection_empty() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::User("hi".into()));
        // Begin at col 50 (way past EOL "hi" which is 2 cols wide).
        // selection_col_range_for_line clamps both ends to width 2,
        // so the effective range is empty.
        r.begin_selection(50, 0);
        r.end_selection();
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            !s.contains("\x1b]52;c;"),
            "no OSC 52 should be emitted for empty selection. got: {:?}",
            s
        );
    }

    /// Begin in the footer area should refuse to anchor a selection.
    /// Anchoring there would bind to a line index that doesn't
    /// exist in body_lines (or worse, points at a row no longer
    /// shown after a scroll), yielding a phantom highlight.
    #[test]
    fn begin_selection_in_footer_does_not_anchor() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::User("hi".into()));
        // body_height = 5, footer starts at row 5. Press at row 7
        // (in the input box / status area).
        r.begin_selection(0, 7);
        assert!(r.selection.selection.is_none(), "footer press must not start a selection");
        assert!(!r.selection.active);
        drop(r);
    }

    /// `update_selection` after `end_selection` is a no-op. JediTerm /
    /// Windows conhost can emit a final coalesced motion event right
    /// after the Up; without `selection_active` gating the head
    /// would jump to that stale point.
    #[test]
    fn update_after_end_does_not_move_head() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::User("hello there".into()));
        r.begin_selection(0, 0);
        r.update_selection(4, 0);
        let head_before_end = r.selection.selection.unwrap().head;
        r.end_selection();
        // Stray motion after release.
        r.update_selection(10, 0);
        let head_after_stray = r.selection.selection.unwrap().head;
        assert_eq!(
            head_before_end, head_after_stray,
            "post-end motion must not move head",
        );
        drop(r);
    }

    /// Selection survives a `end_selection` (so the user can see what
    /// they captured) but a subsequent `reset` (e.g. /clear) wipes it
    /// since body_lines have been emptied — leaving stale indices
    /// would point past end-of-buffer on the next paint.
    #[test]
    fn reset_clears_selection() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::User("hello".into()));
        r.begin_selection(0, 0);
        r.update_selection(3, 0);
        r.end_selection();
        assert!(r.selection.selection.is_some());
        r.reset();
        assert!(r.selection.selection.is_none(), "reset should clear selection");
        drop(r);
    }

    /// `on_resize` clears selection — display columns were anchored
    /// against the old width, after reflow they'd land in the wrong
    /// spots of the painted line.
    #[test]
    fn resize_clears_selection() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::User("hello".into()));
        r.begin_selection(0, 0);
        r.update_selection(3, 0);
        assert!(r.selection.selection.is_some());
        r.on_resize(40, 10);
        assert!(r.selection.selection.is_none(), "resize should clear selection");
        drop(r);
    }

    /// During an active drag, paint emits the reverse-video sequence
    /// over the selected cells. End-to-end check that the click →
    /// drag path actually decorates the body row.
    ///
    /// No menu is rendered in this test, so the only source of
    /// `\x1b[7m` in the buffer is the selection paint.
    #[test]
    fn drag_paints_reverse_video_in_body() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.render(UiLine::User("hello there".into()));
        r.begin_selection(0, 0);
        r.update_selection(4, 0);
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("\x1b[7m"),
            "drag must emit reverse-video. got: {:?}",
            s
        );
    }

    /// Multi-line selection: drag from line 0 col 2 to line 1 col 3
    /// across two body rows. Extracted text should be the cross-row
    /// slice joined by `\n`.
    #[test]
    fn multi_line_drag_extracts_across_rows() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        // Two body rows. body_height = 5; both fit.
        r.body_lines.push("first row".into());
        r.body_lines.push("second row".into());
        r.body_dirty = true;
        // Begin on row 0 of body (= screen row 0 since body_lines.len=2
        // < body_height=5, so viewport_start=0). Drag to row 1, col 3.
        r.begin_selection(2, 0);
        r.update_selection(3, 1);
        let text = r.extract_selection_text();
        // Line 0: from col 2 to EOL of "first row" (9 cols) = "rst row"
        // Line 1: from col 0 to col 4 (head+1) of "second row" = "seco"
        assert_eq!(text, "rst row\nseco", "multi-line extract mismatch: {:?}", text);
        drop(r);
    }

    /// Regression guard for `/language` modal feedback. The picker's
    /// Enter handler emits CommandOutput THEN returns Close; the event
    /// loop then re-renders the input prompt without the menu. The
    /// CommandOutput must survive that second render — otherwise the
    /// user sees no confirmation that the locale switch took effect.
    #[test]
    fn command_output_survives_subsequent_input_prompt_redraw() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);

        // Simulate the exact flow language_picker.rs uses on Enter:
        // first push a confirmation line, then redraw the input prompt
        // without a menu (the event loop's `redraw_idle_plain` after
        // `ModalAction::Close`).
        r.render(UiLine::CommandOutput(
            "  ✓ Language switched to 简体中文 (zh_CN).\n".into(),
        ));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine::default(),
            attachments: Vec::new(),
        });
        r.flush();

        // The body line must still be present in body_lines AND
        // visible in the painted output stream — both layers matter
        // because painting clips out-of-viewport rows.
        let in_body = r
            .body_lines
            .iter()
            .any(|row| row.contains("Language switched to") && row.contains("简体中文"));
        assert!(
            in_body,
            "confirmation line missing from body_lines: {:?}",
            r.body_lines
        );
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("Language switched to") && s.contains("简体中文"),
            "confirmation line missing from painted output: {:?}",
            s
        );
    }

    /// Re-flow on resize: widening the terminal should re-merge previously
    /// split short rows back into fewer longer rows. A single logical
    /// line that was soft-wrapped into 3 chunks at width=10 should
    /// become 1 row after widening to width=80.
    #[test]
    fn resize_wider_merges_split_rows() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 10, 24);
        // Push a 25-char line via push_body_row_raw. At width=10 it
        // wraps into 3 chunks (10 + 10 + 5 chars visible).
        r.push_body_row_raw("abcdefghijklmnopqrstuvwxyz".to_string());
        assert_eq!(
            r.body_lines.len(),
            3,
            "narrow terminal should split 25-char line into 3 rows, got: {:?}",
            r.body_lines
        );
        // Verify raw_body_lines has exactly 1 entry (the original line).
        assert_eq!(
            r.raw_body_lines.len(),
            1,
            "raw_body_lines should have 1 entry, got: {:?}",
            r.raw_body_lines
        );
        // Widen to 80 cols.
        r.on_resize(80, 24);
        // After re-flow the line should fit in a single row.
        assert_eq!(
            r.body_lines.len(),
            1,
            "widened terminal should have 1 row for the 25-char line, got: {:?}",
            r.body_lines
        );
        assert!(
            r.body_lines[0].contains("abcdefghijklmnopqrstuvwxyz"),
            "re-flowed row should contain the original text, got: {:?}",
            r.body_lines[0]
        );
    }

    /// Re-flow on resize: narrowing the terminal should split a
    /// long row into multiple rows instead of truncating it.
    #[test]
    fn resize_narrower_splits_long_rows() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        // Push a 40-char line at width=80 — fits in one row.
        r.push_body_row_raw("a".repeat(40));
        assert_eq!(
            r.body_lines.len(),
            1,
            "80-col terminal should fit 40-char line in 1 row"
        );
        // Narrow to 20 cols.
        r.on_resize(20, 24);
        // After re-flow the line should be split into 2 rows
        // (20 + 20 chars).
        assert_eq!(
            r.body_lines.len(),
            2,
            "narrowed terminal should split 40-char line into 2 rows, got: {:?}",
            r.body_lines
        );
        // Each chunk should be at most 20 visible columns.
        for (i, row) in r.body_lines.iter().enumerate() {
            let w = line_display_width_sgr_aware(row);
            assert!(
                w <= 20,
                "body row {} has display width {} > 20 after resize: {:?}",
                i,
                w,
                row
            );
        }
    }

    /// Re-flow on resize: SGR colour codes in body rows survive
    /// the re-wrap without bleeding into adjacent rows.
    #[test]
    fn resize_reflow_preserves_sgr_colours() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        // Push a coloured line that fits in 80 cols.
        let coloured = format!("{}hello world{}", SGR_CYAN, SGR_RESET);
        r.push_body_row_raw(coloured.clone());
        // Narrow to 5 cols so it wraps.
        r.on_resize(5, 24);
        // Re-flow should produce multiple rows, each containing
        // part of the text.
        assert!(
            r.body_lines.len() > 1,
            "narrowed terminal should split the coloured line, got: {:?}",
            r.body_lines
        );
        // Widen back — the full coloured line should re-merge.
        r.on_resize(80, 24);
        assert_eq!(
            r.body_lines.len(),
            1,
            "widened back should re-merge into 1 row, got: {:?}",
            r.body_lines
        );
        assert!(
            r.body_lines[0].contains("hello world"),
            "re-merged row should contain original text, got: {:?}",
            r.body_lines[0]
        );
    }

    #[test]
    fn alt_message_marks_tracked_on_user_push() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::User("hi".into()));
        assert_eq!(r.message_marks.len(), 1);
        assert_eq!(r.message_marks[0].kind, crate::render::MarkKind::User);
    }

    #[test]
    fn alt_message_marks_decremented_on_drain() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        // Each UiLine::User("line N") pushes 2 body rows (user text + blank spacer):
        //   push_body_row_raw(chevron + text)  → 1 row at width=80
        //   push_body_row(String::new())        → 1 blank row
        // 5005 users → 10010 body rows. The scrollback cap is 5000, so
        // drain = 10010 - 5000 = 5010 rows removed from the front.
        // Marks at line_idx < 5010 are dropped; the first surviving mark
        // was at original idx=5010, which normalises to 0 after subtraction.
        // 5010 / 2 = 2505 marks dropped; 5005 - 2505 = 2500 survive.
        for i in 0..5005 {
            r.render(UiLine::User(format!("line {}", i)));
        }
        assert_eq!(r.message_marks.len(), 2500);
        assert_eq!(r.message_marks[0].line_idx, 0, "first surviving mark should point at body_lines[0] after drain");
    }

    #[test]
    fn alt_scrollbar_paints_thumb_when_enabled_and_overflow() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.show_scrollbar = true;
        for i in 0..30 { r.push_body_row(format!("R{:02}", i)); }
        r.paint_body();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("█"), "thumb char missing: {:?}", s);
    }

    #[test]
    fn alt_scrollbar_not_painted_when_disabled() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        r.show_scrollbar = false;
        for i in 0..30 { r.push_body_row(format!("R{:02}", i)); }
        r.paint_body();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        assert!(!s.contains("█"), "thumb should not appear when disabled: {:?}", s);
    }

    #[test]
    fn alt_scroll_to_prev_message_works_from_sticky_bottom() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        for i in 0..30 { r.render(UiLine::User(format!("u{}", i))); }
        assert!(r.sticky_bottom);
        r.scroll_to_prev_message();
        assert!(!r.sticky_bottom, "alt+up at sticky bottom should leave sticky");
        let max_top = r.body_lines.len().saturating_sub(r.body_height() as usize);
        assert!(r.viewport_top < max_top, "viewport_top should be less than max_top after jumping back");
    }

    #[test]
    fn alt_scroll_to_prev_message_finds_nearest_above() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
        // Populate body with: 5 user lines, then assistant chunks
        for i in 0..5 { r.render(UiLine::User(format!("u{}", i))); }
        for i in 0..5 { r.render(UiLine::AssistantText(format!("a{}", i))); }
        // Scroll to bottom area
        r.scroll_body_to_top();
        r.scroll_body(100); // back down some
        let before = r.viewport_top;
        r.scroll_to_prev_message();
        // Should jump up to some earlier message boundary
        assert!(
            r.viewport_top <= before,
            "viewport should jump up or stay (jumped to prev message): before={} after={}",
            before,
            r.viewport_top
        );
    }
}
