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
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;
use crate::width::{display_width, truncate_to_width};
use unicode_width::UnicodeWidthChar;

/// Truncate `s` to `max_cols` display columns, treating ANSI CSI
/// escape sequences (`\x1b[...{letter}`) as zero-width spans so SGR
/// styling doesn't eat budget that should belong to visible text.
///
/// `truncate_to_width` from `crate::width` counts each character of an
/// SGR sequence (`[`, digits, `m`) as width 1, which under-budgets the
/// visible content — a 79-display-col line decorated with one SGR pair
/// would lose 5+ trailing visible chars even though the line fits the
/// terminal exactly. This helper skips the entire CSI sequence in one
/// go, matching how the terminal interprets it.
///
/// Final SGR reset (`\x1b[0m`) preservation: if truncation cut into an
/// open span, the caller still appends a reset; this fn just guarantees
/// the visible-text count is right.
fn truncate_to_width_sgr_aware(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut acc = String::with_capacity(s.len());
    let mut cols = 0usize;
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        // CSI sequence: ESC `[` {params} {final letter A-Z/a-z}.
        // Append the whole span verbatim (zero visible cost).
        if c == '\x1b' && iter.peek() == Some(&'[') {
            acc.push(c);
            acc.push(iter.next().unwrap()); // consume `[`
            for nc in iter.by_ref() {
                acc.push(nc);
                if nc.is_ascii_alphabetic() {
                    break; // final byte ends the CSI sequence
                }
            }
            continue;
        }
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if cols + w > max_cols {
            break;
        }
        acc.push(c);
        cols += w;
    }
    acc
}

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
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\x1b' && iter.peek() == Some(&'[') {
            // CSI: zero visible width, copy verbatim into current chunk.
            acc.push(c);
            acc.push(iter.next().unwrap());
            for nc in iter.by_ref() {
                acc.push(nc);
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if w > 0 && cols + w > max_cols {
            chunks.push(std::mem::take(&mut acc));
            cols = 0;
        }
        acc.push(c);
        cols += w;
    }
    chunks.push(acc);
    chunks
}

/// Walk `s` and return the visible-text display width, treating CSI
/// escape sequences as zero-width spans (same parser as
/// `truncate_to_width_sgr_aware`). Used to clamp selection columns
/// against the actual painted content of a body line — clicks past the
/// end of the visible row should select nothing in the gap, not extend
/// to the column the user happened to drop on.
fn line_display_width_sgr_aware(s: &str) -> usize {
    let mut cols = 0usize;
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\x1b' && iter.peek() == Some(&'[') {
            iter.next(); // consume `[`
            for nc in iter.by_ref() {
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        cols += UnicodeWidthChar::width(c).unwrap_or(0);
    }
    cols
}

/// Walk `line` and emit it clipped to `max_cols` display columns, with
/// chars whose display column falls in `[sel_start, sel_end)` wrapped
/// in reverse-video (`\x1b[7m` … `\x1b[0m`). CSI escapes outside the
/// selection pass through verbatim so existing colours render; CSI
/// escapes INSIDE the selection are dropped so reverse-video stays
/// solid (otherwise an inline `\x1b[0m` from markdown styling would
/// reset the highlight mid-span).
///
/// Wide chars (CJK, emoji): a single char that straddles `sel_start`
/// or `sel_end` is treated as fully inside if its first column is in
/// range — matches what the user expects when they click on the left
/// half of a wide char.
fn render_line_with_selection(
    line: &str,
    max_cols: usize,
    sel_start: usize,
    sel_end: usize,
) -> String {
    if max_cols == 0 || sel_end <= sel_start {
        return truncate_to_width_sgr_aware(line, max_cols);
    }
    let mut out = String::with_capacity(line.len() + 16);
    let mut cols = 0usize;
    let mut in_sel = false;
    let mut iter = line.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\x1b' && iter.peek() == Some(&'[') {
            // Capture the full CSI span first so we can decide whether
            // to drop it (inside selection) or keep it (outside).
            let mut csi = String::with_capacity(8);
            csi.push(c);
            csi.push(iter.next().unwrap());
            for nc in iter.by_ref() {
                csi.push(nc);
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
            if !in_sel {
                out.push_str(&csi);
            }
            continue;
        }
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if cols >= max_cols {
            break;
        }
        let want_in_sel = cols >= sel_start && cols < sel_end;
        if want_in_sel && !in_sel {
            // Reset existing colours then enable reverse video so the
            // selection highlight is visually consistent regardless of
            // the underlying line styling.
            out.push_str("\x1b[0m\x1b[7m");
            in_sel = true;
        } else if !want_in_sel && in_sel {
            out.push_str("\x1b[0m");
            in_sel = false;
        }
        if cols + w > max_cols {
            break;
        }
        out.push(c);
        cols += w;
    }
    if in_sel {
        out.push_str("\x1b[0m");
    }
    out
}

/// Extract the plain-text characters of `line` whose display column
/// falls in `[sel_start, sel_end)`, dropping all CSI escapes. Used by
/// `extract_selection_text` to assemble what gets written to the
/// clipboard. Wide-char rule matches `render_line_with_selection`.
fn extract_line_selection_text(
    line: &str,
    sel_start: usize,
    sel_end: usize,
) -> String {
    if sel_end <= sel_start {
        return String::new();
    }
    let mut out = String::new();
    let mut cols = 0usize;
    let mut iter = line.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\x1b' && iter.peek() == Some(&'[') {
            iter.next(); // `[`
            for nc in iter.by_ref() {
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if cols >= sel_end {
            break;
        }
        if cols >= sel_start {
            out.push(c);
        }
        cols += w;
    }
    out
}

/// Standard-alphabet base64 encoder. Inline implementation (~30 lines)
/// instead of pulling in the `base64` crate just for OSC 52: the
/// payload is one user-selected text blob per drag-release, kilobytes
/// at most, and the alphabet is fixed.
fn base64_encode(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHA[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => unreachable!(),
    }
    out
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
    /// Cached width / height. Updated by resize in Phase 4.
    width: u16,
    height: u16,
    /// All body rows ever pushed, oldest-first. Each row is a single
    /// physical line of text (with embedded SGR colour escapes).
    /// `paint_body` paints a slice of this against the current viewport;
    /// no terminal-side scrollback is involved (alt-screen owns the
    /// whole viewport, host terminal's scrollback is unreachable).
    body_lines: Vec<String>,
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
    /// True when footer state changed since the last paint. Same role
    /// as `body_dirty` but for the footer strip.
    footer_dirty: bool,
    /// Active mouse-drag selection, or completed selection still
    /// visible until the next interaction. `anchor` is the press
    /// point, `head` is the current drag (or release) point. Both
    /// reference `body_lines` directly: `(line_idx, display_col)` —
    /// so a viewport scroll doesn't desync the selection from its
    /// underlying text. None means no selection rendered. Cleared
    /// on `reset` / `clear_screen` / `on_resize` since each can
    /// invalidate either the line indices (reset) or the display
    /// columns (resize → re-flow at paint time).
    selection: Option<Selection>,
    /// True only between `begin_selection` and `end_selection`. Used
    /// to gate `update_selection` so a stray drag event after the
    /// user already released doesn't move a stale selection. Some
    /// terminals (notably JediTerm) emit a final coalesced motion
    /// event right after Up; without this flag that event would
    /// shift `head` to wherever the cursor was when the buffered
    /// frame arrived.
    selection_active: bool,
}

/// Mouse-drag selection range. See `AltScreenRenderer::selection` for
/// semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    /// (body_line_idx, display_col) anchor — where the press landed.
    anchor: (usize, usize),
    /// (body_line_idx, display_col) head — current drag point.
    /// Equal to anchor immediately after `begin_selection` (zero-
    /// width selection); diverges as drag events extend the range.
    head: (usize, usize),
}

impl Selection {
    /// Return (low, high) where `low <= high` lexicographically. Used
    /// when computing per-line column ranges so paint and copy don't
    /// have to care which way the user dragged.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

impl AltScreenRenderer<BufWriter<Stdout>> {
    pub fn new(caps: TerminalCaps) -> Self {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        Self::with_writer(BufWriter::new(io::stdout()), caps, w, h)
    }
}

impl<W: Write + Send> AltScreenRenderer<W> {
    pub fn with_writer(out: W, caps: TerminalCaps, w: u16, h: u16) -> Self {
        let mut r = Self {
            out,
            caps,
            alt_screen_active: false,
            width: w,
            height: h,
            body_lines: Vec::new(),
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
            footer_dirty: true,
            selection: None,
            selection_active: false,
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
    /// slash-menu palette grows / shrinks the footer dynamically:
    ///   spinner (1) + top_rule (1) + input (1) + bot_rule (1)
    ///   + menu (0..4) + status (1) = 5..9
    fn footer_rows(&self) -> u16 {
        // spinner + top_rule + input + bot_rule + status = 5 base
        5 + self.menu_paint_rows()
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
    ///
    /// Best-effort: if the writer fails, `alt_screen_active` stays
    /// false and Drop won't try to pop.
    fn enter_alt_screen(&mut self) {
        let seq = "\x1b[?1049h\x1b[H\x1b[2J\x1b[?1002h\x1b[?1006h";
        if self.out.write_all(seq.as_bytes()).is_ok() && self.out.flush().is_ok() {
            self.alt_screen_active = true;
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
            let _ = self.out.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1049l");
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
        }
        self.body_dirty = true;
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

        // Walk every row in the visible window. CUP each row, EL to
        // wipe leftover glyphs from previous frames, then write the
        // body content (trimmed to terminal width and SGR-terminated
        // so long lines don't autowrap into the next body row's slot
        // and stale colour spans don't bleed). For rows past the end
        // of body_lines, just EL (clear). 1-indexed rows.
        let max_cols = self.width as usize;
        // Snapshot the ordered selection bounds once so the per-row
        // loop doesn't re-borrow `self.selection` while we hold a
        // reference to `self.body_lines[i]`. Cheap (Copy) and only
        // computed when a selection exists.
        let sel_bounds = self.selection.as_ref().map(|s| s.ordered());
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
        let _ = self.out.flush();
        self.body_dirty = false;
    }

    /// Map a screen-cell `(col, row)` (0-indexed) to a body-line
    /// position `(line_idx, display_col)`. Returns `None` when the
    /// row falls past the last body line (footer area, or the empty
    /// strip below content in early-session views) — used by
    /// `begin_selection` to refuse to anchor a selection in the
    /// footer. `update_selection` calls `screen_to_body_clamped`
    /// instead so dragging past the body still extends the head.
    fn screen_to_body(&self, col: u16, row: u16) -> Option<(usize, usize)> {
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
        Some((line_idx, col as usize))
    }

    /// Same as `screen_to_body` but clamps `(col, row)` to the
    /// nearest valid body cell instead of returning `None`. Used by
    /// `update_selection` so a drag that overshoots into the footer
    /// or past the last row still extends the head sensibly.
    fn screen_to_body_clamped(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        let body_height = self.body_height() as usize;
        let total = self.body_lines.len();
        if total == 0 {
            return None;
        }
        let viewport_start = if self.sticky_bottom {
            total.saturating_sub(body_height)
        } else {
            self.viewport_top.min(total.saturating_sub(body_height))
        };
        let row_clamped = (row as usize).min(body_height.saturating_sub(1));
        let line_idx = (viewport_start + row_clamped).min(total.saturating_sub(1));
        Some((line_idx, col as usize))
    }

    /// Walk the active selection from `start.line` to `end.line` (both
    /// inclusive) and return the concatenated plain text — CSI escapes
    /// stripped, lines joined with `\n`. Returns an empty string when
    /// no selection or when the selection covers no visible chars
    /// (e.g. clicked past end-of-line on a single-line selection).
    fn extract_selection_text(&self) -> String {
        let Some(sel) = self.selection else {
            return String::new();
        };
        let (lo, hi) = sel.ordered();
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

    /// Emit OSC 52 (`\x1b]52;c;<base64>\x07`) carrying `text` so the
    /// host terminal copies it to the system clipboard. Empty text is
    /// a no-op to avoid clearing whatever the user previously had.
    /// Best-effort — terminals that don't honour OSC 52 (Terminal.app
    /// without explicit opt-in) silently ignore the sequence.
    fn write_osc52_clipboard(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let encoded = base64_encode(text.as_bytes());
        let _ = write!(self.out, "\x1b]52;c;{}\x07", encoded);
        let _ = self.out.flush();
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
        let spinner_row = footer_top;
        let top_rule_row = footer_top + 1;
        let input_row = footer_top + 2;
        let bot_rule_row = footer_top + 3;
        let menu_first_row = footer_top + 4;
        let status_row = footer_top + 4 + menu_rows;

        // Row 1 of footer: spinner during streaming, blank otherwise.
        // Frame glyph in brand magenta (Role::Brand), label dim — gives
        // the user a visual anchor as the frame rotates against the
        // dim label.
        let cup = format!("\x1b[{};1H\x1b[K", spinner_row);
        let _ = self.out.write_all(cup.as_bytes());
        if let Some((frame, label)) = &self.pending_spinner {
            let cleaned = scrub_controls(label);
            let line = if self.caps.colors {
                format!(
                    "  {}{}{} {}{}{}",
                    SGR_MAGENTA, frame, SGR_RESET, SGR_DIM, cleaned, SGR_RESET
                )
            } else {
                format!("  {} {}", frame, cleaned)
            };
            let _ = self.out.write_all(line.as_bytes());
        }

        // Top rule: full-width cyan ─ above the input box. Mirrors
        // retained's `build_rule_row`. ASCII fallback to `-` when the
        // terminal can't render unicode glyphs (rare in alt-screen
        // since the auto-fallback target — JediTerm / conhost — both
        // support unicode, but cheap to handle).
        let rule_char = if self.caps.unicode_symbols { "─" } else { "-" };
        let rule = rule_char.repeat(self.width as usize);
        let cup = format!("\x1b[{};1H\x1b[K", top_rule_row);
        let _ = self.out.write_all(cup.as_bytes());
        if self.caps.colors {
            let _ = write!(self.out, "{}{}{}", SGR_CYAN, rule, SGR_RESET);
        } else {
            let _ = self.out.write_all(rule.as_bytes());
        }

        // Input row: `❯ {buf}` flush-left at col 0. matches retained's
        // `build_middle_row`.
        let cup = format!("\x1b[{};1H\x1b[K", input_row);
        let _ = self.out.write_all(cup.as_bytes());
        let chev = self.caps.prompt_chevron();
        let buf_str = self.pending_input.as_ref().map(|(b, _)| b.as_str()).unwrap_or("");
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
        let trimmed = truncate_to_width(&safe_buf, max_cols);
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
                        if selected {
                            format!("▸ /{:<12}  {}", safe_name, safe_desc)
                        } else {
                            format!("  /{:<12}  {}", safe_name, safe_desc)
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
        let status_text = if !self.pending_status.model.is_empty()
            || !self.pending_status.cwd.is_empty()
        {
            let model = scrub_controls(&self.pending_status.model);
            let cwd = scrub_controls(&self.pending_status.cwd);
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
        // user sees where their typing will land.
        if let Some((buf, cursor_byte)) = &self.pending_input {
            let prefix = if *cursor_byte <= buf.len() {
                &buf[..*cursor_byte]
            } else {
                buf.as_str()
            };
            let prefix_safe = scrub_controls(prefix).replace('\n', " ");
            let cursor_col = chev.chars().count() + display_width(&prefix_safe);
            let cup = format!("\x1b[{};{}H\x1b[?25h", input_row, cursor_col + 1);
            let _ = self.out.write_all(cup.as_bytes());
        } else {
            let _ = self.out.write_all(b"\x1b[?25l");
        }

        let _ = self.out.flush();
        self.footer_dirty = false;
    }

    /// Combined frame paint: body first, footer second so the cursor
    /// final-position belongs to the footer (typically the input row).
    ///
    /// Hides the cursor for the duration of the paint so the user
    /// doesn't see it dart through every intermediate CUP. body + footer
    /// emit roughly `body_rows + 5..9` CUP sequences per frame; on
    /// slow terminals (JediTerm in JetBrains IDEs in particular) each
    /// CUP is processed synchronously, and the cursor is briefly
    /// visible at every row 1, 2, 3, …, 7, then through every footer
    /// row before settling. paint_footer's tail emits show-cursor +
    /// the final input-row CUP atomically, so re-revealing it there
    /// gives a single visible position per frame instead of a moving
    /// trail. Reported in Android Studio's terminal as "cursor jumps
    /// around when scrolling history".
    fn paint_frame(&mut self) {
        // Hide cursor up-front so paint_body's per-row CUPs aren't
        // visible to the user. paint_footer's tail re-emits show-
        // cursor (`\x1b[?25h`) at the final input-row position when
        // `pending_input` is set, or leaves it hidden otherwise
        // (e.g. during streaming with no input prompt to anchor on).
        let _ = self.out.write_all(b"\x1b[?25l");
        self.paint_body();
        self.paint_footer();
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
            // so each physical line becomes its own body_lines entry
            // — paint_body assumes one entry == one terminal row.
            for sub in rendered.split('\n') {
                self.push_body_row(sub.to_string());
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
                self.push_body_row(sub.to_string());
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
    /// grain (no Cell layout, just inline SGR). Muted gray colour to
    /// match the existing aesthetic.
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
        // Onboarding hints — first thing a new user reads. Slash
        // shortcuts in cyan accent; surrounding prose in dim text so
        // it reads as subordinate to primary content.
        let hint_a = if self.caps.colors {
            format!(
                "{}type something, or press {}{}/{}{}  to browse commands{}",
                SGR_DIM, SGR_RESET, SGR_CYAN, SGR_RESET, SGR_DIM, SGR_RESET
            )
        } else {
            "type something, or press / to browse commands".into()
        };
        self.push_body_row(hint_a);
        let hint_b = if self.caps.colors {
            format!(
                "{}/provider{}  {}to add a custom model{}",
                SGR_CYAN, SGR_RESET, SGR_DIM, SGR_RESET
            )
        } else {
            "/provider  to add a custom model".into()
        };
        self.push_body_row(hint_b);
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
            self.push_body_row(row);
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
        self.push_body_row(row);
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
        self.push_body_row(row);
    }

    /// `[Error: ...]` row. Red when colours on. Mirrors PlainRenderer.
    fn push_error(&mut self, msg: &str) {
        self.flush_assistant_remainder();
        let safe = scrub_controls(msg);
        let row = if self.caps.colors {
            format!("{}[Error: {}]{}", SGR_RED, safe, SGR_RESET)
        } else {
            format!("[Error: {}]", safe)
        };
        self.push_body_row(row);
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
        self.push_body_row(row);
    }

    /// `(cancelled)` marker row.
    fn push_cancelled(&mut self) {
        self.flush_assistant_remainder();
        let row = if self.caps.colors {
            format!("{}(cancelled){}", SGR_DIM, SGR_RESET)
        } else {
            "(cancelled)".to_string()
        };
        self.push_body_row(row);
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
        self.push_body_row(row);
    }

    /// Push CommandOutput verbatim, splitting on newlines so each
    /// physical line is its own body row.
    fn push_command_output(&mut self, text: &str) {
        self.flush_assistant_remainder();
        let safe = scrub_controls(text);
        // Soft-wrap each line at the current terminal width. Without this,
        // `paint_body` truncates long lines (e.g. the OAuth URL in
        // /login) at the right edge — invisible content can't be
        // selected for copy. Wrapping splits one logical row into N body
        // rows so every glyph stays on screen and selectable.
        let max_w = (self.width as usize).max(1);
        for line in safe.split('\n') {
            for chunk in wrap_to_width_sgr_aware(line, max_w) {
                self.push_body_row(chunk);
            }
        }
    }
}

/// Compute the half-open column range `[start, end)` of `line` that
/// falls inside the ordered selection bounds `(lo, hi)`. Returns
/// `None` if the line is outside the row range. Bounds within the
/// line are clamped to the visible display width so a click past the
/// end doesn't extend selection into thin air.
///
/// Free function (rather than a method) so the body-paint loop can
/// call it while holding a borrow of `self.body_lines[i]` without
/// re-borrowing `self`.
fn selection_col_range_for_line(
    line_idx: usize,
    lo: (usize, usize),
    hi: (usize, usize),
    line: &str,
) -> Option<(usize, usize)> {
    if line_idx < lo.0 || line_idx > hi.0 {
        return None;
    }
    let line_w = line_display_width_sgr_aware(line);
    let start_col = if line_idx == lo.0 { lo.1 } else { 0 };
    // Line containing the head: include the cell under the head —
    // half-open `end_col` = head_col + 1. Middle lines select to
    // end of line; the bottom line of a multi-line selection uses
    // the same `hi.1 + 1` rule as a same-line selection.
    let end_col_exclusive = if line_idx == hi.0 {
        hi.1.saturating_add(1)
    } else {
        line_w
    };
    let s = start_col.min(line_w);
    let e = end_col_exclusive.min(line_w);
    if e <= s {
        return None;
    }
    Some((s, e))
}

impl<W: Write + Send> Renderer for AltScreenRenderer<W> {
    fn render(&mut self, line: UiLine) {
        match line {
            // ── body: welcome / turn events ──
            UiLine::Welcome { model, working_dir } => {
                self.push_welcome(&model, &working_dir);
            }
            UiLine::User(text) => {
                self.push_user(&text);
            }
            UiLine::TurnSeparator { label } => {
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
                self.push_tool_call(&name, &detail);
            }
            UiLine::ToolCallCommit { .. } => {
                // Phase 3 will add live-spinner freezing here. Phase 2
                // pushes ToolCallInFlight as a static row already, so
                // there's nothing to freeze yet.
            }
            UiLine::ToolGroupRender { batch_id: _, header, children } => {
                // alt-screen mirrors retained's append-style without
                // the in-place ✓ rewrite (alt-screen layout is
                // virtual-buffer based; live-group rewrite would need
                // its own row tracking). Header + children print
                // statically; ChildUpdate appends a new row.
                self.push_command_output(&header);
                for c in children {
                    self.push_command_output(&c.text);
                }
            }
            UiLine::ToolGroupChildUpdate { batch_id: _, call_id: _, new_text } => {
                self.push_command_output(&new_text);
            }
            UiLine::ToolGroupSummary { text } => {
                self.push_command_output(&text);
            }
            UiLine::ToolResult { success, summary } => {
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
                let safe_tool = scrub_controls(&tool);
                let safe_detail = scrub_controls(&detail);
                let prompt = format!(
                    "Allow {}({})? [Y]es / [N]o / [A]lways",
                    safe_tool, safe_detail
                );
                let row = if self.caps.colors {
                    format!("{}{}{}", SGR_CYAN, prompt, SGR_RESET)
                } else {
                    prompt
                };
                self.push_body_row(row);
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
            } => {
                self.pending_input = Some((buf, cursor_byte));
                self.pending_status = status;
                self.pending_menu = menu; // slash-palette payload
                self.pending_spinner = None; // input takes over from spinner
                self.footer_dirty = true;
                // Menu state changes the footer height (variable rows).
                // Repaint body too so it shrinks/grows correspondingly.
                self.body_dirty = true;
            }
            UiLine::StreamingBox {
                buf,
                cursor_byte,
                frame,
                label,
                status,
                menu,
            } => {
                self.pending_input = Some((buf, cursor_byte));
                self.pending_status = status;
                self.pending_menu = menu;
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
        self.selection = None;
        self.selection_active = false;
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
        //   2. wipe the alt-screen with `\x1b[2J\x1b[H` so stale
        //      pre-resize glyphs at old absolute positions can't
        //      ghost — iTerm2 / some terminals leave them visible
        //      until something overwrites them
        //   3. mark both panes dirty + repaint
        //
        // body_lines are kept verbatim; on resize-narrower the next
        // paint_body truncates each row to the new width, on
        // resize-wider previously-clipped tails reappear from the
        // un-truncated source. No re-flow / re-wrap needed because
        // we trim at paint time, not at push time.
        self.width = cols;
        self.height = rows;
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
        self.selection = None;
        self.selection_active = false;
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        self.body_dirty = true;
        self.footer_dirty = true;
        self.paint_frame();
    }

    fn begin_selection(&mut self, col: u16, row: u16) {
        // Only anchor a selection when the press lands inside the
        // body region. Footer / blank-area presses clear any prior
        // selection (so a stray click also acts as "deselect").
        match self.screen_to_body(col, row) {
            Some(pos) => {
                self.selection = Some(Selection { anchor: pos, head: pos });
                self.selection_active = true;
            }
            None => {
                self.selection = None;
                self.selection_active = false;
            }
        }
        self.body_dirty = true;
        self.paint_frame();
    }

    fn update_selection(&mut self, col: u16, row: u16) {
        // Guard against terminals that emit a coalesced motion event
        // right after Up — without this, that stale motion would
        // shift `head` of an already-finalised selection.
        if !self.selection_active {
            return;
        }
        let Some(pos) = self.screen_to_body_clamped(col, row) else {
            return;
        };
        if let Some(sel) = self.selection.as_mut() {
            if sel.head == pos {
                return; // no-op move (cell-granularity, drag jitter)
            }
            sel.head = pos;
            self.body_dirty = true;
            self.paint_frame();
        }
    }

    fn end_selection(&mut self) {
        // Mark the selection as finalised but keep it visible so the
        // user can see what they captured. A subsequent press starts
        // a fresh selection (or deselects on footer/empty hit).
        self.selection_active = false;
        let text = self.extract_selection_text();
        self.write_osc52_clipboard(&text);
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

    /// Phase 3.5: code fences toggle md_state. A ```fenced``` block
    /// keeps subsequent lines in code-block mode (no inline markdown
    /// applied). The fence line itself returns None from render_line
    /// (no body row pushed for the ```fence``` line) but `body_dirty`
    /// is still set on subsequent content.
    #[test]
    fn fenced_code_block_state_carries_across_streaming_chunks() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
        r.render(UiLine::AssistantText("```rust\n".into()));
        // Fence line itself doesn't render — md_state.in_code_block
        // flips to true, no body row pushed.
        assert_eq!(r.body_lines.len(), 0, "fence-open line must not push");
        assert!(r.md_state.in_code_block, "code-block state must flip on");

        r.render(UiLine::AssistantText("let x = 1;\n".into()));
        assert_eq!(r.body_lines.len(), 1, "code line pushed");
        // Code-block state still on — next line should still be in code.
        assert!(r.md_state.in_code_block);

        r.render(UiLine::AssistantText("```\n".into()));
        // Fence-close, state flips back.
        assert!(!r.md_state.in_code_block, "code-block state must flip off");
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

    /// Phase 4.5: top + bottom rules render as cyan ─ across full width.
    #[test]
    fn input_box_has_top_and_bottom_rules() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 20, 10);
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: crate::render::StatusLine::default(),
        });
        r.flush();
        drop(r);
        let s = String::from_utf8_lossy(&buf);
        // top_rule at row 7, bot_rule at row 9. Each row has 20 ─.
        let twenty_dashes = "─".repeat(20);
        assert!(s.contains("\x1b[7;1H"), "top rule row CUP missing. got: {:?}", s);
        assert!(s.contains("\x1b[9;1H"), "bot rule row CUP missing. got: {:?}", s);
        assert!(
            s.contains(&twenty_dashes),
            "20 ─ chars missing. got: {:?}",
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
    /// magenta (`\x1b[95m`) when caps.colors is on — visual anchor so
    /// the rotation reads as motion against the dim label. Mirrors
    /// `RetainedRenderer::build_spinner_body_row` (Role::Brand frame +
    /// Role::Secondary label).
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
        // Label still dim — the two SGRs co-exist on the same row.
        assert!(s.contains("\x1b[2m"), "label should still be dim. got: {:?}", s);
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
            },
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
            },
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

    /// Regression: every paint_frame must start by hiding the cursor
    /// so its journey through ~10+ intermediate CUP positions (one
    /// per body row, one per footer row) isn't visible to the user.
    /// Synchronous-CUP terminals like JediTerm rendered the cursor's
    /// trail as visible "jumping" — Android Studio bug report.
    /// paint_footer re-emits show-cursor at its tail when
    /// pending_input is set, so the cursor only appears once at its
    /// final position.
    #[test]
    fn paint_frame_hides_cursor_before_painting() {
        let mut buf = Vec::new();
        let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
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

    /// `line_display_width_sgr_aware` returns the visible-width of a
    /// styled line. SGR escapes are zero-cost; CJK chars are 2 cols.
    /// Sanity check that the helpers used by the selection paint
    /// don't double-count colour escapes.
    #[test]
    fn line_display_width_skips_sgr() {
        assert_eq!(line_display_width_sgr_aware("hello"), 5);
        assert_eq!(line_display_width_sgr_aware("\x1b[31mhello\x1b[0m"), 5);
        assert_eq!(line_display_width_sgr_aware("中文"), 4);
        assert_eq!(line_display_width_sgr_aware("\x1b[1m中\x1b[0m文"), 4);
    }

    /// `extract_line_selection_text` should return only the chars
    /// whose display column falls in `[start, end)`, with all CSI
    /// escapes dropped — that's what gets written to the clipboard.
    /// Visible cols of `"\x1b[31mhello\x1b[0m world"` are
    /// `h=0 e=1 l=2 l=3 o=4 ' '=5 w=6 o=7 r=8 l=9 d=10`.
    #[test]
    fn extract_line_selection_strips_sgr_and_clips_to_range() {
        let line = "\x1b[31mhello\x1b[0m world";
        assert_eq!(extract_line_selection_text(line, 0, 5), "hello");
        assert_eq!(extract_line_selection_text(line, 6, 11), "world");
        // crosses the SGR boundary: cols 3..8 = "lo wo"
        assert_eq!(extract_line_selection_text(line, 3, 8), "lo wo");
        // empty range
        assert_eq!(extract_line_selection_text(line, 5, 5), "");
        // out-of-bounds end clips to last visible col
        assert_eq!(extract_line_selection_text(line, 7, 100), "orld");
    }

    /// `render_line_with_selection` wraps the selected range in
    /// reverse-video and ends it with a reset. CSI escapes outside
    /// the selection pass through verbatim; CSI escapes inside the
    /// selection are dropped so the highlight stays solid.
    #[test]
    fn render_line_with_selection_emits_reverse_video() {
        let line = "hello world";
        let out = render_line_with_selection(line, 80, 0, 5);
        assert!(out.starts_with("\x1b[0m\x1b[7m"), "should open with reset+reverse. got: {:?}", out);
        assert!(out.contains("hello"), "selected text missing. got: {:?}", out);
        assert!(out.contains("\x1b[0m world"), "post-selection plain text missing. got: {:?}", out);
    }

    /// A CSI escape *inside* the selection range must be dropped
    /// (otherwise an inline `\x1b[0m` from markdown styling would
    /// tear a hole in the highlight by closing the reverse-video
    /// span mid-selection).
    ///
    /// Visible cols of `"he\x1b[31mre\x1b[0m"` are `h=0 e=1 r=2 e=3`.
    /// Select [0, 4) — both interior CSI escapes (`\x1b[31m` between
    /// cols 1-2 and `\x1b[0m` after col 3) must be stripped.
    #[test]
    fn render_line_with_selection_drops_inline_csi_inside_range() {
        let line = "he\x1b[31mre\x1b[0m";
        let out = render_line_with_selection(line, 80, 0, 4);
        assert!(
            !out.contains("\x1b[31m"),
            "inline red CSI inside selection should be dropped. got: {:?}",
            out
        );
        // Reset count: open-reset at selection start + close-reset
        // at selection end. The interior `\x1b[0m` from the source
        // line MUST be dropped; if it leaked through we'd see 3.
        let resets = out.matches("\x1b[0m").count();
        assert_eq!(resets, 2, "expected open-reset + close-reset only. got: {:?}", out);
    }

    /// Empty selection range collapses to a plain SGR-aware truncate.
    /// Guards `selection_col_range_for_line` returning `None` from
    /// upstream — the path that calls `render_line_with_selection`
    /// shouldn't, but if it ever did the visual would just be the
    /// unhighlighted line.
    #[test]
    fn render_line_with_empty_selection_is_plain_truncate() {
        let line = "hello world";
        assert_eq!(render_line_with_selection(line, 80, 5, 5), "hello world");
    }

    /// `selection_col_range_for_line` clamps to the visible width
    /// of the line — clicking past EOL on a one-line selection
    /// shouldn't extend the range past the last visible col.
    #[test]
    fn selection_range_clamps_to_line_width() {
        // 5-col line. Anchor at col 0, head at col 100 → [0, 5).
        let r = selection_col_range_for_line(0, (0, 0), (0, 100), "hello");
        assert_eq!(r, Some((0, 5)));
        // Anchor past EOL → None.
        let r = selection_col_range_for_line(0, (0, 50), (0, 100), "hello");
        assert_eq!(r, None);
    }

    /// Multi-line selection: first line covers [start_col, EOL],
    /// middle lines fully selected, last line covers [0, head_col+1].
    #[test]
    fn selection_range_multi_line_shape() {
        // Three lines, anchor at (0, 3), head at (2, 2). Lines are
        // "first", "middle", "last".
        let lo = (0, 3);
        let hi = (2, 2);
        assert_eq!(
            selection_col_range_for_line(0, lo, hi, "first"),
            Some((3, 5)),
            "first line [3, 5) — from col 3 to EOL",
        );
        assert_eq!(
            selection_col_range_for_line(1, lo, hi, "middle"),
            Some((0, 6)),
            "middle line fully selected",
        );
        assert_eq!(
            selection_col_range_for_line(2, lo, hi, "last"),
            Some((0, 3)),
            "last line [0, head+1) = [0, 3)",
        );
        // Lines outside [lo.0, hi.0] return None.
        assert_eq!(selection_col_range_for_line(3, lo, hi, "outside"), None);
    }

    /// Base64 round-trip on the standard alphabet, including padding
    /// for non-multiple-of-3 inputs. OSC 52 expects exactly this
    /// encoding (the `c` selector is the system clipboard).
    #[test]
    fn base64_encode_matches_standard_alphabet() {
        // Empty.
        assert_eq!(base64_encode(b""), "");
        // 1 byte → 2 chars + 2 pad.
        assert_eq!(base64_encode(b"f"), "Zg==");
        // 2 bytes → 3 chars + 1 pad.
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        // 3 bytes → no pad.
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        // 4 bytes → 6 chars + 2 pad.
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        // RFC 4648 vector.
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

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
        let expected = format!("\x1b]52;c;{}\x07", base64_encode(b"hello"));
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
        assert!(r.selection.is_none(), "footer press must not start a selection");
        assert!(!r.selection_active);
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
        let head_before_end = r.selection.unwrap().head;
        r.end_selection();
        // Stray motion after release.
        r.update_selection(10, 0);
        let head_after_stray = r.selection.unwrap().head;
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
        assert!(r.selection.is_some());
        r.reset();
        assert!(r.selection.is_none(), "reset should clear selection");
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
        assert!(r.selection.is_some());
        r.on_resize(40, 10);
        assert!(r.selection.is_none(), "resize should clear selection");
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
}
