// crates/atomcode-tuix/src/render/ansi.rs
use std::io::{BufWriter, Stdout, Write};

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;

use super::theme::{role, Role};
use super::{Renderer, UiLine};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;

/// Outer margin in columns on both left and right of the whole UI. All
/// content (prose, tool lines, markdown, footer box, menu) is inset by this
/// many columns — flush-to-edge text is visually jarring, especially on
/// wider terminals.
const PAD_COL: usize = 2;

/// Format a running token count as a compact human label — "842 tokens",
/// "1.2k tokens", "12.3k tokens". Status bar is width-constrained so we
/// avoid long raw numbers.
fn format_token_count(n: usize) -> String {
    if n < 1000 {
        format!("{} tokens", n)
    } else {
        format!("{:.1}k tokens", (n as f64) / 1000.0)
    }
}

// ── SGR helpers that append to a String (so arms can build a full line
// buffer and emit it through the single wrapping path). ──

fn push_sgr_fg(buf: &mut String, caps: TerminalCaps, r: Role) {
    use std::fmt::Write as _;
    if let Some(color) = role(caps, r) {
        // Delegate to crossterm's SGR encoding — it emits the correct
        // sequence for basic 16-color variants (e.g. Color::DarkGrey →
        // `\x1b[90m`), 256-color, and truecolor without us having to
        // branch per variant.
        let _ = write!(buf, "{}", crossterm::style::SetForegroundColor(color));
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

/// Footer-state snapshot used to redraw the bottom chrome after content writes.
#[derive(Clone, Default)]
struct FooterState {
    buf: String,
    cursor_byte: usize,
    /// Rendered menu items (already paged to at most 4).
    menu_items: Vec<(String, String)>,
    /// Index into menu_items that should display selected. 0-indexed.
    menu_selected_in_view: Option<usize>,
    spinner_frame: Option<String>,
    spinner_label: Option<String>,
    /// Persistent status line under the box.
    status: super::StatusLine,
    /// Rows to walk up from the cursor resting position (box middle at
    /// row K) to reach the footer's top row. Populated by
    /// `draw_footer_here` so `erase_footer` knows exactly how far to
    /// `\x1b[?A` regardless of whether the box is 1 row or N rows tall.
    cursor_row_from_top: usize,
}

/// ANSI renderer — PURE APPEND architecture (no scroll region).
///
/// Layout (relative to current cursor position, not absolute rows):
///   ┌──────────────────────────────┐
///   │ content (scrollback)         │  ← everything flows here
///   │ ...                          │
///   │ [blank margin row]           │  ← 1 row between content and box
///   │ ╭──────────────────╮         │  ← box top
///   │ │ ❯ {user input}   │         │  ← box middle (cursor here)
///   │ ╰──────────────────╯         │  ← box bottom
///   │ menu row 1 (if active)       │
///   │ menu row 2 (if active)       │
///   │ menu row 3 (if active)       │
///   │ menu row 4 (if active)       │
///   └──────────────────────────────┘
///
/// Render cycle: erase_footer → emit content \r\n → draw_footer_here.
/// Terminal scrolls naturally when cursor hits the bottom row.
pub struct AnsiRenderer<W: Write + Send> {
    out: W,
    caps: TerminalCaps,
    /// Number of rows the currently-drawn footer occupies. 0 if footer
    /// not yet drawn (pre-welcome) or already erased.
    footer_rows: usize,
    /// Last footer state passed to draw_footer_here. Permanent content
    /// emits redraw the footer using this snapshot.
    last_footer: FooterState,
    /// Pre-encoded byte slice for each row of the last painted footer.
    /// `draw_footer_here` compares each new row against this and only
    /// emits `\x1b[2K` + new content for rows that actually changed,
    /// letting unchanged rules / status / menu rows sit on screen with
    /// zero bytes sent. Empty vec = no prior footer (cold start).
    last_footer_rows: Vec<Vec<u8>>,
    /// Tracks if we're mid-assistant-text block (next text delta should NOT
    /// re-emit the "  │ " prefix for the first line).
    assistant_continuing: bool,
    /// Buffer for the current assistant-text line. Deltas accumulate here
    /// until a '\n' arrives.
    assistant_line_buf: String,
    /// Markdown parser state (code-block tracking, table row buffering).
    md_state: crate::markdown::MdState,
    /// Per-frame throttle for InputPrompt / StreamingBox. Smooths
    /// keystroke storms that outrun Terminal.app's ANSI-processing
    /// budget — see `render::throttle` for the full rationale.
    throttle: super::throttle::InputThrottle,
    /// Active DECSTBM scroll region `(top, bottom)`, 1-indexed. `None`
    /// means no region is set (initial state, or cleared for external
    /// hijack). Fixed-footer architecture: we keep the region as
    /// `[1, H - footer_rows]` so body content scrolls in the upper area
    /// and the footer at `[H - footer_rows + 1, H]` is untouched by
    /// body writes — streaming TextDelta no longer cold-starts the
    /// footer, killing the 700B-per-delta Mac Terminal.app saturation.
    scroll_region: Option<(u16, u16)>,
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
            footer_rows: 0,
            last_footer: FooterState::default(),
            last_footer_rows: Vec::new(),
            assistant_continuing: false,
            assistant_line_buf: String::new(),
            md_state: crate::markdown::MdState::new(),
            throttle: super::throttle::InputThrottle::new(),
            scroll_region: None,
        }
    }

    /// Current terminal dimensions (`(width, height)`), falling back
    /// to 80×24 if crossterm can't probe (pipe, dumb, test env).
    fn term_size(&self) -> (u16, u16) {
        crossterm::terminal::size().unwrap_or((80, 24))
    }

    /// Last row of the scroll region (= first row outside the footer,
    /// inclusive on the top side). 1-indexed. Content writes stream
    /// here; `\r\n` at this row triggers SU inside the region.
    fn scroll_bottom(&self) -> u16 {
        let (_, h) = self.term_size();
        h.saturating_sub(self.footer_rows as u16).max(1)
    }

    /// Absolute 1-indexed row of the footer's top. Footer occupies
    /// `[footer_top(), H]`. Invalid when `footer_rows == 0`.
    fn footer_top(&self) -> u16 {
        let (_, h) = self.term_size();
        h.saturating_sub(self.footer_rows as u16).saturating_add(1)
    }

    /// True when the DECSTBM fixed-footer path is active for this
    /// render call. Requires a terminal that supports scroll regions
    /// AND a footer that has been drawn at least once.
    fn decstbm_active(&self) -> bool {
        self.caps.scroll_region && self.footer_rows > 0
    }

    /// Ensure the terminal's scroll region matches `[1, scroll_bottom]`.
    /// Idempotent — skips the DECSTBM write if already current.
    /// Called at every footer redraw so footer-height changes (menu
    /// open/close, multi-line input) transparently re-flow.
    fn sync_scroll_region(&mut self) {
        if !self.caps.scroll_region || self.footer_rows == 0 {
            return;
        }
        let bottom = self.scroll_bottom();
        let want = (1u16, bottom);
        if self.scroll_region == Some(want) {
            return;
        }
        let _ = write!(self.out, "\x1b[{};{}r", want.0, want.1);
        self.scroll_region = Some(want);
        crate::tuix_trace!("DECSTBM", "set 1..{} (footer_rows={})", bottom, self.footer_rows);
    }

    /// Release any active scroll region (`\x1b[r`). Call before any
    /// path that hands the terminal off — suspend_for_external,
    /// shutdown, panic cleanup. Leaving DECSTBM set after exit means
    /// the user's shell inherits a truncated scroll area and behaves
    /// weirdly (scrolling breaks below former footer row).
    fn clear_scroll_region(&mut self) {
        if self.scroll_region.is_none() {
            return;
        }
        let _ = self.out.write_all(b"\x1b[r");
        self.scroll_region = None;
        crate::tuix_trace!("DECSTBM", "clear");
    }

    /// Recompute the input cursor's absolute (row, col) from
    /// `last_footer` and reposition. Called at the end of every body
    /// write in DECSTBM mode so the blinking cursor visibly returns
    /// to the input box after streaming text lands. Without this the
    /// cursor would sit in the scroll region below the last body line.
    fn reposition_cursor_to_input(&mut self) {
        if !self.decstbm_active() {
            return;
        }
        let top = self.footer_top();
        let w = self.term_width();
        let rule_width = w.saturating_sub(PAD_COL * 2);
        let text_budget = rule_width.saturating_sub(2);
        let safe = scrub_controls(&self.last_footer.buf);
        let (_, cursor_row_in_middle, cursor_col_in_row) = if text_budget == 0 {
            (vec![String::new()], 0usize, 0usize)
        } else {
            crate::width::wrap_with_cursor(&safe, text_budget, self.last_footer.cursor_byte)
        };
        // Row 0 = spinner, Row 1 = top rule, Row 2 = first middle line.
        let abs_row = top + 2 + cursor_row_in_middle as u16;
        let abs_col = (PAD_COL + 2 + cursor_col_in_row + 1) as u16;
        let _ = write!(self.out, "\x1b[{};{}H", abs_row, abs_col);
    }

    /// Emit a block of body lines into the scroll region. Pure-append
    /// semantics preserved: each line pushes older content up, footer
    /// at `[footer_top(), H]` is untouched. Used by every
    /// content-write path (ToolCall, ToolResult, streaming text,
    /// DiffBlock, TurnSeparator, etc.) when DECSTBM is active.
    ///
    /// Each logical line may wrap into multiple visible rows — we
    /// scroll once per visible row, so wrapping is transparent.
    /// Caller is responsible for the eventual cursor reposition
    /// (or a draw_footer redraw) — this method leaves the cursor at
    /// the bottom of the scroll region, column matching the last
    /// wrapped chunk length.
    fn emit_body_lines_decstbm(&mut self, lines: &[String]) {
        let bottom = self.scroll_bottom();
        let w = self.content_width();
        // Park cursor at bottom of scroll region. Each `\r\n` at this
        // row triggers SU inside `[1, bottom]` — content moves up,
        // bottom becomes empty-ready for the next chunk.
        let _ = write!(self.out, "\x1b[{};1H", bottom);
        for line in lines {
            // A logical line may itself contain `\n` (markdown renderer
            // returns multi-row bodies for code blocks / tables). Split
            // and wrap each physical row independently.
            for phys in line.split('\n') {
                for chunk in crate::width::wrap_line_to_width(phys, w) {
                    // Always scroll first so we never overwrite the
                    // bottom row's previous contents.
                    let _ = self.out.write_all(b"\r\n");
                    self.write_left_pad();
                    let _ = self.out.write_all(chunk.as_bytes());
                }
            }
        }
    }

    /// Unified body-emit helper used by every content-write path
    /// (ToolCall, ToolResult, DiffLine/Block, streaming markdown, etc.).
    /// Branches on DECSTBM availability: fixed-footer ⇒ scroll-region
    /// emit + cursor-only repost; legacy ⇒ erase + emit + redraw.
    fn emit_body_block(&mut self, lines: &[String]) {
        if self.decstbm_active() {
            self.emit_body_lines_decstbm(lines);
            self.reposition_cursor_to_input();
            return;
        }
        self.erase_footer();
        let w = self.content_width();
        for line in lines {
            for phys in line.split('\n') {
                for chunk in crate::width::wrap_line_to_width(phys, w) {
                    self.write_left_pad();
                    let _ = self.out.write_all(chunk.as_bytes());
                    let _ = self.out.write_all(b"\r\n");
                }
            }
        }
        self.redraw_footer_if_any();
    }

    /// Paint any deferred InputPrompt / StreamingBox. Called by
    /// `flush_deferred` (on the event-loop's 20ms timer) and by any
    /// immediate render that needs to preserve paint order (footer
    /// must redraw below the content write).
    ///
    /// **Must flush after dispatch.** The event loop's `renderer.render()
    /// + renderer.flush()` pair is how bytes normally reach the terminal;
    /// a parked paint drained by the 5ms deferred tick has no matching
    /// `renderer.flush()` from the caller, so without this explicit flush
    /// the ANSI bytes sit in BufWriter's 8KB buffer and don't show up on
    /// screen until the *next* user-triggered render forces a flush.
    /// Symptom: type "你好好" via IME, see only "你" until pressing any
    /// subsequent key — that key's own render.flush() finally pushes the
    /// parked "你好好" bytes out.
    fn paint_pending_input(&mut self) {
        if let Some(line) = self.throttle.take_pending() {
            self.dispatch_unthrottled(line);
            self.throttle.mark_painted();
            let _ = self.out.flush();
        }
    }

    /// Render a UiLine bypassing the throttle. Used internally by
    /// `render` after it decides a line is not throttled (or the
    /// throttle window has elapsed), and by `paint_pending_input`.
    fn dispatch_unthrottled(&mut self, line: UiLine) {
        // Pure dispatch. Every variant has its own method; adding a new
        // UiLine means "add a method + one arm here", not "grow an
        // already-1000-line match".
        match line {
            UiLine::Welcome { model, working_dir } => self.render_welcome_line(&model, &working_dir),
            UiLine::User(text) => self.render_user_line(&text),
            UiLine::AssistantText(text) => self.render_assistant_text(&text),
            UiLine::AssistantLineBreak => self.render_assistant_line_break(),
            UiLine::ToolCall { name, detail } => self.render_tool_call(&name, &detail),
            UiLine::ToolResult { success, summary } => self.render_tool_result(success, &summary),
            UiLine::DiffLine { added, text } => self.render_diff_line(added, &text),
            UiLine::DiffBlock(entries) => self.render_diff_block(&entries),
            UiLine::ApprovalPrompt { tool, detail } => self.render_approval_prompt(&tool, &detail),
            UiLine::Error(msg) => self.render_error_line(&msg),
            UiLine::TurnCancelled => self.render_turn_cancelled(),
            UiLine::TurnComplete => self.render_turn_complete(),
            UiLine::Spinner { frame, label } => self.render_spinner(frame, &label),
            UiLine::StreamingBox { buf, cursor_byte, frame, label, status, menu } =>
                self.render_streaming_box(&buf, cursor_byte, frame, &label, status, menu),
            UiLine::ClearTransient => { /* no-op: footer is fixed-at-bottom */ }
            UiLine::InputPrompt { buf, cursor_byte, menu, status } =>
                self.render_input_prompt(&buf, cursor_byte, menu, status),
            UiLine::InputCommit => { /* no-op: ClearTransient + User handles commit */ }
            UiLine::TurnSeparator { label } => self.render_turn_separator(&label),
            UiLine::CommandOutput(text) => self.render_command_output(&text),
        }
    }

    /// Erase the currently-drawn footer. Cursor is on the box middle row
    /// at the K-th middle line (0-based); distance from there up to the
    /// footer top is `2 + K` (row 0 = spinner/blank, row 1 = ╭─╮ border,
    /// rows 2..2+N-1 = middle). `draw_footer_here` populates
    /// `last_footer.cursor_row_from_top` so we know the exact number
    /// regardless of how tall the box is.
    fn erase_footer(&mut self) -> usize {
        if self.footer_rows == 0 {
            return 0;
        }
        // DECSTBM mode: footer is pinned at absolute rows via the
        // scroll region. Body writes stream INTO the scroll region
        // above and never touch the footer, so "erase the footer
        // before writing content" is a no-op here. Critically, we
        // also preserve `last_footer_rows` — in DECSTBM mode the
        // footer stays on screen byte-for-byte across content writes,
        // so the next redraw can legitimately diff-skip unchanged rows.
        // This is the core mechanism that brings streaming TextDelta
        // from ~700 B/delta down to body-only bytes.
        if self.caps.scroll_region {
            return 0;
        }
        let t0 = std::time::Instant::now();
        let up = self.last_footer.cursor_row_from_top.max(1);
        let was_rows = self.footer_rows;
        let seq = format!("\x1b[{}A\r\x1b[J", up);
        let bytes = seq.len();
        let _ = self.out.write_all(seq.as_bytes());
        self.footer_rows = 0;
        // Legacy (non-DECSTBM) path: next paint starts from a blank
        // slate, so caches that would "diff same" against wiped-off
        // rows must be invalidated.
        self.last_footer_rows.clear();
        crate::tuix_trace!(
            "FOOT",
            "erase up={} rows={} bytes={} dur={}µs",
            up,
            was_rows,
            bytes,
            t0.elapsed().as_micros()
        );
        bytes
    }

    /// Draw the footer starting at the current cursor position. Layout:
    ///
    ///   row 0:         spinner line ("⠋ Pondering") — or blank margin
    ///   row 1:         ╭─────────────╮   box top
    ///   row 2..2+N-1:  │ ❯ line 1   │   middle row (cursor lands on row 2+K)
    ///                  │   line 2   │   extra middle rows when buf wraps
    ///   row 2+N:       ╰─────────────╯   box bottom
    ///   row 3+N..3+N+M: menu items (M ≤ 4)
    ///
    /// The first middle row carries the "❯ " prompt glyph; wrapped
    /// continuation rows use "  " (two spaces) in that position so text
    /// keeps its indent. Box auto-grows in height as the user types past
    /// the right border — no horizontal scroll.
    /// Cold-start wrapper: called after `erase_footer` has moved the cursor
    /// to the footer top and cleared `last_footer_rows`. Passes 0 as the
    /// "previous cursor row" because the previous row cache is empty and
    /// the cursor is already at footer row 0.
    fn draw_footer_here(&mut self) {
        self.draw_footer_here_with_prev_cursor(0);
    }

    /// Paint the footer at the current cursor position, diffing against
    /// `last_footer_rows`. `prev_cursor_row` is the row offset (within the
    /// previously-drawn footer) the cursor is currently at — emit_footer_diff
    /// uses it to walk the cursor up to the footer top before diffing.
    fn draw_footer_here_with_prev_cursor(&mut self, prev_cursor_row: usize) {
        let t0 = std::time::Instant::now();
        let state = self.last_footer.clone();
        let w = self.term_width();
        // CC-style footer: the input area is framed by a horizontal rule
        // above and below (like an <hr>), with no vertical sides.
        let rule_width = w.saturating_sub(PAD_COL * 2);
        // Text budget = rule width minus the "❯ " prompt prefix (or the
        // equivalent "  " on wrapped continuation rows).
        let text_budget = rule_width.saturating_sub(2);

        // Wrap the buffer and locate the cursor in the wrapped layout.
        let safe = scrub_controls(&state.buf);
        let (mut lines, cursor_row_in_middle, cursor_col_in_row) = if text_budget == 0 {
            (vec![String::new()], 0usize, 0usize)
        } else {
            crate::width::wrap_with_cursor(&safe, text_budget, state.cursor_byte)
        };
        if lines.is_empty() {
            lines.push(String::new());
        }
        let middle_rows = lines.len();

        // ── Build every row into its own pre-encoded byte buffer ──
        // These are compared against `last_footer_rows` so unchanged
        // rows don't produce any terminal output. Rows are built
        // deterministically from state so byte-equality == visual-
        // equality (no stale SGR state leaking between rows because
        // every builder ends with an explicit reset).
        let mut new_rows: Vec<Vec<u8>> = Vec::with_capacity(8);

        // Row 0: spinner (if present) or blank margin.
        new_rows.push(self.build_spinner_row(&state));
        // Row 1: top horizontal rule.
        new_rows.push(self.build_rule_row(rule_width));
        // Rows 2..2+N-1: middle input lines.
        for (i, line) in lines.iter().enumerate() {
            new_rows.push(self.build_middle_row(line, i == 0));
        }
        // Row 2+N: bottom horizontal rule.
        new_rows.push(self.build_rule_row(rule_width));
        // Rows 3+N..: menu items (0-4).
        let menu_rows = state.menu_items.len().min(4);
        for (i, (name, desc)) in state.menu_items.iter().take(4).enumerate() {
            let selected = state.menu_selected_in_view == Some(i);
            new_rows.push(self.build_menu_row(name, desc, selected, rule_width));
        }
        // Status row (if any chrome to show).
        let has_status = !state.status.model.is_empty()
            || !state.status.cwd.is_empty()
            || state.status.hint.is_some();
        let status_rows = if has_status { 1 } else { 0 };
        if has_status {
            new_rows.push(self.build_status_row(&state.status, rule_width));
        }

        let total_rows = new_rows.len();
        let cursor_row_from_top = 2 + cursor_row_in_middle;

        let prev_total_rows = self.footer_rows;
        // Commit the new footer height up-front so `sync_scroll_region`,
        // `scroll_bottom`, and `footer_top` see the new layout when they
        // compute absolute row numbers.
        self.footer_rows = total_rows;

        // DECSTBM: update scroll region boundary BEFORE painting so any
        // stale cursor positioning inside the region honours the new
        // lower bound. Also wipes cache rows if footer just shrank —
        // handled inside `emit_footer_absolute`.
        if self.caps.scroll_region {
            self.sync_scroll_region();
        }

        let (changed_rows, bytes) = if self.caps.scroll_region {
            self.emit_footer_absolute(
                &new_rows,
                prev_total_rows,
                total_rows,
                cursor_row_from_top,
                PAD_COL + 2 + cursor_col_in_row,
            )
        } else {
            self.emit_footer_diff(
                &new_rows,
                prev_cursor_row,
                cursor_row_from_top,
                PAD_COL + 2 + cursor_col_in_row,
            )
        };

        // Save state for next diff.
        self.last_footer.cursor_row_from_top = cursor_row_from_top;
        self.last_footer_rows = new_rows;

        // bytes = sum of emitted row byte counts + positioning overhead.
        // Typical incremental paint ~30-80 bytes (middle row only). A
        // cold-start paint (post-erase_footer) sits at ~600+ bytes since
        // every row is "new" vs empty cache — watch for these during
        // streaming TextDelta bursts.
        crate::tuix_trace!(
            "FOOT",
            "draw rule_w={} mid={} menu={} status={} total={} changed={} bytes={} dur={}µs",
            rule_width,
            middle_rows,
            menu_rows,
            status_rows,
            total_rows,
            changed_rows,
            bytes,
            t0.elapsed().as_micros()
        );
    }

    // ── Per-row builders ──
    //
    // Each returns the exact byte sequence that, when emitted at column 0
    // of the target row (after an erase-line), paints the row identically
    // to the old imperative code. Rows are self-contained: every colour
    // change is paired with a reset so SGR state doesn't bleed across
    // row boundaries when the diff skips intermediate rows.

    fn build_spinner_row(&self, state: &FooterState) -> Vec<u8> {
        let mut s = String::new();
        if let (Some(frame), Some(label)) = (state.spinner_frame.as_ref(), state.spinner_label.as_ref()) {
            for _ in 0..PAD_COL { s.push(' '); }
            push_sgr_fg(&mut s, self.caps, Role::Brand);
            s.push_str(frame);
            s.push(' ');
            push_sgr_fg_reset(&mut s, self.caps);
            push_sgr_bold_on(&mut s, self.caps);
            push_sgr_fg(&mut s, self.caps, Role::Secondary);
            s.push_str(&scrub_controls(label));
            push_sgr_fg_reset(&mut s, self.caps);
            push_sgr_bold_off(&mut s, self.caps);
        }
        s.into_bytes()
    }

    fn build_rule_row(&self, rule_width: usize) -> Vec<u8> {
        let mut s = String::new();
        for _ in 0..PAD_COL { s.push(' '); }
        push_sgr_fg(&mut s, self.caps, Role::Border);
        for _ in 0..rule_width { s.push('─'); }
        push_sgr_fg_reset(&mut s, self.caps);
        s.into_bytes()
    }

    fn build_middle_row(&self, line: &str, is_first: bool) -> Vec<u8> {
        let mut s = String::new();
        for _ in 0..PAD_COL { s.push(' '); }
        if is_first {
            push_sgr_fg(&mut s, self.caps, Role::Accent);
            s.push_str("❯ ");
            push_sgr_fg_reset(&mut s, self.caps);
        } else {
            s.push_str("  ");
        }
        s.push_str(line);
        s.into_bytes()
    }

    fn build_menu_row(&self, name: &str, desc: &str, selected: bool, rule_width: usize) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut s = String::new();
        for _ in 0..PAD_COL { s.push(' '); }
        if selected {
            // Reverse video so contrast works on any terminal theme.
            if self.caps.colors {
                s.push_str("\x1b[7m");
                s.push_str("\x1b[1m");
            }
            let _ = write!(s, "  ▸ /{:<12}  {}", name, desc);
            // Pad to rule_width so highlight strip visually reaches the
            // rule's right edge above/below.
            let content_w = 5 + name.chars().count() + 2 + desc.chars().count();
            let right_pad = rule_width.saturating_sub(content_w);
            for _ in 0..right_pad { s.push(' '); }
            if self.caps.colors {
                s.push_str("\x1b[0m");
            }
        } else {
            push_sgr_fg(&mut s, self.caps, Role::Muted);
            let _ = write!(s, "    /{:<12}  {}", name, desc);
            push_sgr_fg_reset(&mut s, self.caps);
        }
        s.into_bytes()
    }

    fn build_status_row(&self, status: &super::StatusLine, rule_width: usize) -> Vec<u8> {
        let mut s = String::new();
        for _ in 0..PAD_COL { s.push(' '); }
        push_sgr_fg(&mut s, self.caps, Role::Muted);
        let mut parts: Vec<String> = Vec::with_capacity(3);
        if !status.model.is_empty() {
            parts.push(scrub_controls(&status.model));
        }
        if !status.cwd.is_empty() {
            parts.push(scrub_controls(&status.cwd));
        }
        if status.total_tokens > 0 {
            parts.push(format_token_count(status.total_tokens));
        }
        let left = parts.join(" · ");
        let max = rule_width.max(1);

        if let Some(raw_hint) = status.hint.as_deref() {
            let hint = scrub_controls(raw_hint);
            let hint_w = crate::width::display_width(&hint);
            // If hint alone exceeds the row, drop it — don't truncate
            // model/cwd chrome which is the primary info.
            if hint_w + 1 < max {
                let left_budget = max - hint_w - 1;
                let left_truncated = crate::width::truncate_to_width(&left, left_budget);
                let left_w = crate::width::display_width(&left_truncated);
                let pad = max - left_w - hint_w;
                s.push_str(&left_truncated);
                for _ in 0..pad { s.push(' '); }
                push_sgr_fg_reset(&mut s, self.caps);
                push_sgr_fg(&mut s, self.caps, Role::Error);
                s.push_str(&hint);
            } else {
                let truncated = crate::width::truncate_to_width(&left, max);
                s.push_str(&truncated);
            }
        } else {
            let truncated = crate::width::truncate_to_width(&left, max);
            s.push_str(&truncated);
        }
        push_sgr_fg_reset(&mut s, self.caps);
        s.into_bytes()
    }

    /// DECSTBM fixed-footer paint: each row drawn at its absolute
    /// screen row `\x1b[{row};1H`, row-level diff against
    /// `last_footer_rows` preserved so unchanged rows stay at 0 bytes
    /// even across content-write events (cache is no longer invalidated
    /// by `erase_footer` because body writes never erase — they stream
    /// into the scroll region above).
    ///
    /// Footer occupies `[footer_top, H]`, 1-indexed. If footer just
    /// shrank (`prev_total_rows > new_total_rows`), the rows formerly
    /// occupied by footer but now in scroll territory are explicitly
    /// wiped so stale characters don't ghost under freshly scrolling
    /// body text.
    fn emit_footer_absolute(
        &mut self,
        new_rows: &[Vec<u8>],
        prev_total_rows: usize,
        new_total_rows: usize,
        target_cursor_row_in_footer: usize,
        target_cursor_col_from_edge: usize,
    ) -> (usize, usize) {
        let (_, h) = self.term_size();
        let new_top = h.saturating_sub(new_total_rows as u16).saturating_add(1);
        let mut bytes = 0usize;

        // If footer just shrank, the old top rows that are now in
        // scroll territory still hold stale footer glyphs. Wipe them
        // one by one so body content doesn't render atop ghost rules.
        if prev_total_rows > new_total_rows {
            let prev_top = h
                .saturating_sub(prev_total_rows as u16)
                .saturating_add(1);
            for row in prev_top..new_top {
                let s = format!("\x1b[{};1H\x1b[2K", row);
                let _ = self.out.write_all(s.as_bytes());
                bytes += s.len();
            }
        }

        // Disable autowrap for the paint — rule rows emit exactly
        // `rule_width` cells and we don't want a UTF-8 terminal's
        // autowrap to shift the next row down.
        let _ = self.out.write_all(b"\x1b[?7l");
        bytes += 4;

        let prev = std::mem::take(&mut self.last_footer_rows);
        let mut changed = 0usize;
        // When the footer grew (prev_total_rows < new_total_rows), the
        // new top rows have no cache entry → emit unconditionally.
        // When rows match in bytes, skip.
        let growth = new_total_rows.saturating_sub(prev_total_rows);
        for (i, new_row) in new_rows.iter().enumerate() {
            // Old cache index: since footer grew at the top (newer rows
            // get prepended when menu opens / input multi-lines), the
            // prev[i] for i < growth doesn't correspond to the same
            // visual row. Simpler invariant: treat all prev cache as
            // stale when total_rows changed — cache only trusted when
            // sizes match. Over-emit in that one frame, then stable.
            let prev_row = if prev_total_rows == new_total_rows && growth == 0 {
                prev.get(i)
            } else {
                None
            };
            if prev_row != Some(new_row) {
                let abs_row = new_top + i as u16;
                let s = format!("\x1b[{};1H\x1b[2K", abs_row);
                let _ = self.out.write_all(s.as_bytes());
                bytes += s.len();
                let _ = self.out.write_all(new_row);
                bytes += new_row.len();
                changed += 1;
            }
        }

        let _ = self.out.write_all(b"\x1b[?7h");
        bytes += 4;

        // Park cursor at the input cell (row = footer_top + offset,
        // col = left pad + "❯ " + col in wrapped input).
        let cursor_abs_row = new_top + target_cursor_row_in_footer as u16;
        let cursor_abs_col = target_cursor_col_from_edge as u16 + 1; // 1-indexed
        let s = format!("\x1b[{};{}H", cursor_abs_row, cursor_abs_col);
        let _ = self.out.write_all(s.as_bytes());
        bytes += s.len();

        (changed, bytes)
    }

    /// Diff the newly built rows against `last_footer_rows` and emit
    /// `\x1b[2K` + content only for rows whose bytes changed. Returns
    /// the number of rows actually emitted (for trace).
    ///
    /// `prev_cursor_row` is where the cursor currently sits relative to
    /// the footer top (from the previous paint). Used to walk up before
    /// diffing. Pass 0 when `erase_footer` has already put the cursor
    /// at the footer top AND cleared `last_footer_rows` — in that case
    /// prev is empty and no walk-up is needed.
    fn emit_footer_diff(
        &mut self,
        new_rows: &[Vec<u8>],
        prev_cursor_row: usize,
        target_cursor_row: usize,
        target_cursor_col: usize,
    ) -> (usize, usize) {
        let prev = std::mem::take(&mut self.last_footer_rows);
        let total_rows = new_rows.len();
        let max_rows = total_rows.max(prev.len());

        let mut bytes = 0usize;

        // Disable autowrap for the whole paint — stray wrap at the
        // right edge would shift cursor down and desynchronise our
        // row tracking.
        let _ = self.out.write_all(b"\x1b[?7l");
        bytes += 4;

        // Position cursor at (row 0, col 0) of the footer area.
        // When `prev` is non-empty, cursor was left at `prev_cursor_row`
        // within the previous footer — walk it up to row 0.
        // When `prev` is empty (cold start post-erase_footer), cursor
        // is already at row 0; just CR to guarantee col 0.
        if !prev.is_empty() && prev_cursor_row > 0 {
            let s = format!("\x1b[{}A", prev_cursor_row);
            bytes += s.len();
            let _ = self.out.write_all(s.as_bytes());
        }
        let _ = self.out.write_all(b"\r");
        bytes += 1;

        let mut changed = 0usize;
        for i in 0..max_rows {
            let new_row = new_rows.get(i);
            let prev_row = prev.get(i);
            let emit = match (new_row, prev_row) {
                (Some(n), Some(p)) => n != p,
                (Some(_), None) => true,   // new row beyond prev footer
                (None, Some(_)) => true,   // prev had row, new doesn't — erase
                (None, None) => false,
            };
            if emit {
                // Erase the whole line first (clears whatever was
                // there in the prev paint), then lay down new bytes.
                // For `None` new_row (row disappearing), just erase.
                let _ = self.out.write_all(b"\x1b[2K");
                bytes += 4;
                if let Some(n) = new_row {
                    let _ = self.out.write_all(n);
                    bytes += n.len();
                }
                changed += 1;
            }
            // Advance to next row if any rows remain.
            if i + 1 < max_rows {
                let _ = self.out.write_all(b"\r\n");
                bytes += 2;
            }
        }

        // Cursor now at row (max_rows - 1), arbitrary col. Move back up
        // to target row, then set col.
        let last_row = max_rows.saturating_sub(1);
        if last_row > target_cursor_row {
            let s = format!("\x1b[{}A", last_row - target_cursor_row);
            bytes += s.len();
            let _ = self.out.write_all(s.as_bytes());
        }
        let _ = self.out.write_all(b"\r");
        bytes += 1;
        if target_cursor_col > 0 {
            // \x1b[NG is 1-indexed column positioning.
            let s = format!("\x1b[{}G", target_cursor_col + 1);
            bytes += s.len();
            let _ = self.out.write_all(s.as_bytes());
        }

        let _ = self.out.write_all(b"\x1b[?7h");
        bytes += 4;
        (changed, bytes)
    }

    /// Redraw footer if it was previously drawn — used after permanent
    /// content writes to put the box back.
    fn redraw_footer_if_any(&mut self) {
        if !self.last_footer.buf.is_empty()
            || !self.last_footer.menu_items.is_empty()
            || self.last_footer.spinner_frame.is_some()
            || !self.last_footer.status.model.is_empty()
            || self.footer_rows > 0
        {
            self.draw_footer_here();
        }
    }

    /// Back-compat shim for arms that used to call this name. In the
    /// pure-append model, "moving to scroll bottom" = erasing the footer
    /// and leaving cursor at the content-emit position.
    fn move_to_scroll_bottom(&mut self) {
        self.erase_footer();
    }

    /// Update last_footer state and redraw at current cursor. Everything
    /// about layout lives in draw_footer_here — this is just a dispatcher.
    fn draw_footer_with_menu(
        &mut self,
        buf: &str,
        cursor_byte: usize,
        spinner: Option<(&str, &str)>,
        menu: Option<&super::MenuPayload>,
        status: super::StatusLine,
    ) {
        // Paginate menu to the currently-visible 4 items.
        let (menu_items, selected_in_view) = if let Some(m) = menu {
            let len = m.items.len();
            if len == 0 {
                (Vec::new(), None)
            } else {
                // Keep selected in view: compute offset such that selected
                // sits somewhere in [offset, offset+4).
                let offset = if len <= 4 {
                    0
                } else if m.selected < 4 {
                    0
                } else {
                    (m.selected + 1).saturating_sub(4).min(len.saturating_sub(4))
                };
                let end = (offset + 4).min(len);
                let items: Vec<(String, String)> = m.items[offset..end].to_vec();
                let sel = if m.selected >= offset && m.selected < end {
                    Some(m.selected - offset)
                } else {
                    None
                };
                (items, sel)
            }
        } else {
            (Vec::new(), None)
        };

        let (sp_frame, sp_label) = match spinner {
            Some((f, l)) if menu.is_none() => (Some(f.to_string()), Some(l.to_string())),
            _ => (None, None),
        };

        // IN-PLACE footer update (keystroke, spinner tick, menu nav):
        // do NOT call `erase_footer`. Erasing would wipe the screen
        // AND clear `last_footer_rows` — defeating the diff that lets
        // unchanged rules / menu / status rows stay at zero bytes.
        //
        // Instead, capture the previous frame's cursor_row_from_top so
        // `emit_footer_diff` can move the cursor back to the footer top
        // from wherever the last paint left it.
        let prev_cursor_row = self.last_footer.cursor_row_from_top;

        self.last_footer = FooterState {
            buf: buf.to_string(),
            cursor_byte,
            menu_items,
            menu_selected_in_view: selected_in_view,
            spinner_frame: sp_frame,
            spinner_label: sp_label,
            status,
            // cursor_row_from_top populated by draw_footer_here below.
            cursor_row_from_top: 0,
        };

        self.draw_footer_here_with_prev_cursor(prev_cursor_row);
    }

    /// Back-compat wrapper — routes to draw_footer_with_menu(menu=None).
    fn draw_footer(
        &mut self,
        buf: &str,
        cursor_byte: usize,
        spinner: Option<(&str, &str)>,
        status: super::StatusLine,
    ) {
        self.draw_footer_with_menu(buf, cursor_byte, spinner, None, status);
    }

    // Shim so existing call sites keep compiling. Scroll-region mode makes
    // transient clearing largely unnecessary, but permanent arms still call
    // these before writing — route them to move_to_scroll_bottom.
    fn clear_line_if_needed(&mut self) {
        if !self.assistant_continuing {
            self.move_to_scroll_bottom();
        }
    }
    /// Effective content width — terminal width minus left+right padding
    /// and a 1-col safety margin against autowrap at the absolute rightmost
    /// column. Always ≥ 1 so wrapping never collapses to zero.
    fn content_width(&self) -> usize {
        self.term_width()
            .saturating_sub(PAD_COL * 2 + 1)
            .max(1)
    }

    /// Emit PAD_COL spaces to the stdout at the start of a line.
    fn write_left_pad(&mut self) {
        for _ in 0..PAD_COL {
            let _ = self.out.write_all(b" ");
        }
    }

    /// Central path for emitting one logical permanent line. Three steps:
    ///   1. erase_footer — remove the box/menu so content writes to a
    ///      clean area.
    ///   2. wrap & emit content — each chunk prefixed with left pad and
    ///      suffixed with \r\n; terminal scrolls naturally when cursor
    ///      passes bottom row.
    ///   3. redraw footer — blank margin + box + menu at new cursor
    ///      position (below the content we just wrote).
    fn emit_wrapped_line(&mut self, line: &str) {
        self.emit_body_block(&[line.to_string()]);
    }

    fn emit_blank_line(&mut self) {
        self.emit_body_block(&[String::new()]);
    }

    // clear_old_footer_rows removed — static footer layout means no
    // transitions between region-interior and region-exterior.

    /// Flush any complete lines (those ending in '\n') from
    /// `assistant_line_buf` to stdout with inline markdown applied.
    /// Partial last line stays buffered.
    ///
    /// **Batched render cycle:** a single `TextDelta` event can easily
    /// carry a whole markdown paragraph with 20+ internal `\n`s. The
    /// naive per-line approach — erase_footer + emit + redraw_footer
    /// for each — produces 20× the ANSI traffic and blocks the event
    /// loop long enough to freeze the spinner task (the task can't
    /// deliver ticks while event_loop is mid-write). Instead we drain
    /// *all* complete lines, do one erase, write the bodies, and one
    /// redraw. Handler time drops from O(N × 4KB) to O(N × 200B).
    fn flush_assistant_lines(&mut self) {
        if !self.assistant_line_buf.contains('\n') {
            return;
        }
        // Collect complete lines first; render_line can mutate md_state
        // (table buffering), so we must iterate deterministically.
        let mut bodies: Vec<String> = Vec::new();
        while let Some(nl) = self.assistant_line_buf.find('\n') {
            let line: String = self.assistant_line_buf.drain(..=nl).collect();
            let content = &line[..line.len() - 1];
            if let Some(rendered) =
                crate::markdown::render_line(content, &mut self.md_state, self.caps)
            {
                bodies.push(rendered);
            }
        }
        if bodies.is_empty() {
            return;
        }
        self.emit_body_block(&bodies);
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
            self.emit_body_block(&[block]);
        }
    }

    /// Write a complete assistant line through the inline markdown
    /// renderer, then push the rendered body through the unified
    /// `emit_body_block` helper.
    fn write_assistant_rendered_line(&mut self, content: &str) {
        let Some(rendered) = crate::markdown::render_line(
            content, &mut self.md_state, self.caps,
        ) else {
            return;
        };
        self.emit_body_block(&[rendered]);
    }

    fn term_width(&self) -> usize {
        crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80)
    }

    fn render_welcome(&mut self, model: &str, working_dir: &str) {
        // Compact layout: 6 rows, 2-space indent, single-blank separator
        // to match CC's density. Builds each row as a pre-formatted string
        // (with its own SGR bracketing) and pushes the batch through
        // `emit_body_block`, which handles the left pad so in DECSTBM
        // mode these rows stream into the scroll region and leave the
        // fixed footer untouched.
        let model = scrub_controls(model);
        let working_dir = scrub_controls(working_dir);
        let w = self.term_width();
        let caps = self.caps;
        let mut lines: Vec<String> = Vec::with_capacity(6);

        // Row 1: brand on the left, "v{ver}  ·  MIT" right-aligned.
        // `content_w` = width inside the left + right pad; `write_left_pad`
        // contributes PAD_COL spaces when emit_body_block fires.
        let content_w = w.saturating_sub(PAD_COL * 2);
        let left_txt = "◆ AtomCode";
        let right_ver = "v4.18.1";
        let right_lic = "MIT";
        let left_w = crate::width::display_width(left_txt);
        let right_w = right_ver.len() + 5 + right_lic.len(); // "  ·  "
        let gap = content_w.saturating_sub(left_w + right_w);
        let mut row = String::new();
        push_sgr_bold_on(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::Brand);
        row.push_str(left_txt);
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_bold_off(&mut row, caps);
        for _ in 0..gap {
            row.push(' ');
        }
        push_sgr_fg(&mut row, caps, Role::Secondary);
        row.push_str(right_ver);
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::Muted);
        row.push_str("  ·  ");
        row.push_str(right_lic);
        push_sgr_fg_reset(&mut row, caps);
        lines.push(row);

        // Row 2: ∙ cwd
        let max_path = w.saturating_sub(6);
        let cwd_disp = crate::width::truncate_to_width(&working_dir, max_path);
        let mut row = String::new();
        push_sgr_fg(&mut row, caps, Role::AccentDim);
        row.push_str("∙ ");
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::Secondary);
        row.push_str(&cwd_disp);
        push_sgr_fg_reset(&mut row, caps);
        lines.push(row);

        // Row 3: ∙ model
        let model_disp = crate::width::truncate_to_width(&model, max_path);
        let mut row = String::new();
        push_sgr_fg(&mut row, caps, Role::AccentDim);
        row.push_str("∙ ");
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::Secondary);
        row.push_str(&model_disp);
        push_sgr_fg_reset(&mut row, caps);
        lines.push(row);

        // Row 4: blank separator
        lines.push(String::new());

        // Row 5: "type something, or press / to browse commands"
        let mut row = String::new();
        push_sgr_fg(&mut row, caps, Role::AccentDim);
        row.push_str("type something, or press  ");
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_bold_on(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::Accent);
        row.push('/');
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_bold_off(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::AccentDim);
        row.push_str("  to browse commands");
        push_sgr_fg_reset(&mut row, caps);
        lines.push(row);

        // Row 6: "/provider to add a custom model"
        let mut row = String::new();
        push_sgr_bold_on(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::Accent);
        row.push_str("/provider");
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_bold_off(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::AccentDim);
        row.push_str("  to add a custom model");
        push_sgr_fg_reset(&mut row, caps);
        lines.push(row);

        self.emit_body_block(&lines);
    }

    // ── UiLine variant handlers ──
    // One method per UiLine variant. Each is the body of the old match
    // arm, unchanged. Renderer::render is now pure dispatch.

    fn render_welcome_line(&mut self, model: &str, working_dir: &str) {
        self.render_welcome(model, working_dir);
        self.assistant_continuing = false;
    }

    fn render_user_line(&mut self, text: &str) {
        let safe = scrub_controls(text);
        let caps = self.caps;
        // CC-style echo: accent prompt glyph + plain text, one trailing
        // blank row for separation from the assistant response.
        let mut row = String::new();
        push_sgr_bold_on(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::Accent);
        row.push_str("❯ ");
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_bold_off(&mut row, caps);
        row.push_str(&safe);
        self.emit_body_block(&[row, String::new()]);
        self.assistant_continuing = false;
        // New user turn → reset markdown parser state.
        self.md_state.reset();
    }

    fn render_assistant_text(&mut self, text: &str) {
        // Line-buffered: accumulate until \n boundaries, then render
        // each complete line through inline markdown.
        let safe = scrub_controls(text);
        self.assistant_line_buf.push_str(&safe);
        self.flush_assistant_lines();
        self.assistant_continuing = !self.assistant_line_buf.is_empty();
    }

    fn render_assistant_line_break(&mut self) {
        self.flush_assistant_remainder();
        self.assistant_continuing = false;
    }

    fn render_tool_call(&mut self, name: &str, detail: &str) {
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
        line.push_str(&scrub_controls(name));
        push_sgr_fg_reset(&mut line, self.caps);
        push_sgr_bold_off(&mut line, self.caps);
        if !detail.is_empty() {
            push_sgr_fg(&mut line, self.caps, Role::Muted);
            line.push('(');
            line.push_str(&scrub_controls(detail));
            line.push(')');
            push_sgr_fg_reset(&mut line, self.caps);
        }
        self.emit_wrapped_line(&line);
    }

    fn render_tool_result(&mut self, success: bool, summary: &str) {
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
        line.push_str(&scrub_controls(summary));
        push_sgr_fg_reset(&mut line, self.caps);
        self.emit_wrapped_line(&line);
        // Paragraph spacer.
        self.emit_blank_line();
    }

    fn render_diff_line(&mut self, added: bool, text: &str) {
        let mut line = String::new();
        push_sgr_fg(&mut line, self.caps,
            if added { Role::DiffAdd } else { Role::DiffRemove });
        let sign = if added { '+' } else { '-' };
        line.push_str(&format!("       {} {}", sign, scrub_controls(text)));
        push_sgr_fg_reset(&mut line, self.caps);
        self.emit_wrapped_line(&line);
    }

    fn render_diff_block(&mut self, entries: &[super::DiffEntry]) {
        // Build one String per entry, then push the batch through the
        // unified body emitter. 50 entries still result in a single
        // erase/redraw cycle (legacy) or a single scroll-region sweep
        // (DECSTBM) — the event loop stays unblocked for the spinner.
        let mut bodies: Vec<String> = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut line = String::new();
            push_sgr_fg(
                &mut line,
                self.caps,
                if entry.added { Role::DiffAdd } else { Role::DiffRemove },
            );
            let sign = if entry.added { '+' } else { '-' };
            line.push_str(&format!(
                "       {} {}",
                sign,
                scrub_controls(&entry.text)
            ));
            push_sgr_fg_reset(&mut line, self.caps);
            bodies.push(line);
        }
        self.emit_body_block(&bodies);
    }

    fn render_approval_prompt(&mut self, tool: &str, detail: &str) {
        let mut line = String::new();
        push_sgr_fg(&mut line, self.caps, Role::Warning);
        line.push_str(&format!(
            "  Allow {}({})? [Y]es / [N]o / [A]lways",
            scrub_controls(tool), scrub_controls(detail)
        ));
        push_sgr_fg_reset(&mut line, self.caps);
        self.emit_wrapped_line(&line);
    }

    fn render_error_line(&mut self, msg: &str) {
        if self.assistant_continuing || !self.assistant_line_buf.is_empty() {
            self.flush_assistant_remainder();
            self.assistant_continuing = false;
        }
        let mut line = String::new();
        push_sgr_fg(&mut line, self.caps, Role::Error);
        line.push_str(&format!("  [Error: {}]", scrub_controls(msg)));
        push_sgr_fg_reset(&mut line, self.caps);
        self.emit_wrapped_line(&line);
        self.assistant_continuing = false;
    }

    fn render_turn_cancelled(&mut self) {
        let mut line = String::new();
        push_sgr_fg(&mut line, self.caps, Role::Muted);
        line.push_str("  (cancelled)");
        push_sgr_fg_reset(&mut line, self.caps);
        self.emit_wrapped_line(&line);
        self.assistant_continuing = false;
    }

    fn render_turn_complete(&mut self) {
        // flush_assistant_remainder does erase+emit+redraw, leaving
        // cursor at box middle. TurnSeparator (emitted right after
        // this by the event loop) provides the blank line above itself,
        // so we don't add one here — doing so would drift the cursor
        // away from box middle and break the next erase_footer's
        // "up 2" calibration.
        self.flush_assistant_remainder();
        self.assistant_continuing = false;
    }

    fn render_spinner(&mut self, frame: &'static str, label: &str) {
        // Legacy path — map to the fixed footer with spinner.
        if self.assistant_continuing {
            return;
        }
        self.draw_footer("", 0, Some((frame, label)), self.last_footer.status.clone());
    }

    fn render_streaming_box(
        &mut self,
        buf: &str,
        cursor_byte: usize,
        frame: &'static str,
        label: &str,
        status: super::StatusLine,
        menu: Option<super::MenuPayload>,
    ) {
        if self.assistant_continuing {
            return;
        }
        // When the user is typing `/` mid-stream, show the command
        // palette in place of the spinner — otherwise keep the legacy
        // spinner-only path.
        if menu.is_some() {
            self.draw_footer_with_menu(buf, cursor_byte, Some((frame, label)), menu.as_ref(), status);
        } else {
            self.draw_footer(buf, cursor_byte, Some((frame, label)), status);
        }
    }

    fn render_input_prompt(
        &mut self,
        buf: &str,
        cursor_byte: usize,
        menu: Option<super::MenuPayload>,
        status: super::StatusLine,
    ) {
        self.draw_footer_with_menu(buf, cursor_byte, None, menu.as_ref(), status);
    }

    fn render_turn_separator(&mut self, label: &str) {
        let inner_w = self.term_width().saturating_sub(PAD_COL * 2);
        let safe = scrub_controls(label);
        let lw = crate::width::display_width(&safe);
        let padded = 1 + lw + 1;
        let remaining = inner_w.saturating_sub(padded);
        let left = remaining / 2;
        let right = remaining - left;
        let caps = self.caps;

        let mut row = String::new();
        push_sgr_fg(&mut row, caps, Role::Muted);
        for _ in 0..left { row.push('─'); }
        row.push(' ');
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::Secondary);
        row.push_str(&safe);
        push_sgr_fg_reset(&mut row, caps);
        push_sgr_fg(&mut row, caps, Role::Muted);
        row.push(' ');
        for _ in 0..right { row.push('─'); }
        push_sgr_fg_reset(&mut row, caps);

        // Blank row above + separator + blank row below so the rule
        // breathes between the tool output it closes and the prompt
        // that follows.
        self.emit_body_block(&[String::new(), row, String::new()]);
    }

    fn render_command_output(&mut self, text: &str) {
        let safe = scrub_controls(text);
        for phys in safe.split('\n') {
            self.emit_wrapped_line(phys);
        }
    }
}

impl<W: Write + Send> Renderer for AnsiRenderer<W> {
    fn render(&mut self, line: UiLine) {
        // InputThrottle removed: it was protecting Mac Terminal from
        // the old 1500-byte full-redraw bursts. Phase 2's row-diff
        // cuts per-keystroke paints to ~80 bytes, which Mac Terminal
        // ingests in under a millisecond — so parking the 2nd of 3
        // rapid IME chars for 5-10ms now shows up as visible stutter
        // instead of smoothing a storm that no longer exists.
        //
        // `paint_pending_input` is still called below to drain any
        // payload parked by code that pre-dates this change (e.g.
        // `flush_deferred` tick), keeping the upgrade backward-safe.
        self.paint_pending_input();
        self.dispatch_unthrottled(line);
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn shutdown(&mut self) {
        // Clear any multi-line transient (input box) cleanly.
        self.clear_line_if_needed();
        // Release the DECSTBM fixed-footer scroll region. If we exit
        // with a restricted region still active, the user's shell
        // inherits the truncated scroll area and everything below the
        // former footer row silently fails to scroll — a very confusing
        // "my terminal is broken" experience.
        self.clear_scroll_region();
        let _ = self.out.write_all(b"\r\n");
        let _ = self.out.flush();
    }

    fn reset(&mut self) {
        // Release DECSTBM BEFORE the screen wipe — clearing a scroll
        // region after a `\x1b[2J` is harmless, but some emulators
        // misbehave if we leave a region set while the cursor is being
        // moved to (1,1).
        self.clear_scroll_region();
        // Wipe the physical terminal + cursor home so the next render
        // starts from a known (row 1, col 1) position.
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        // Forget everything we cached about the prior footer — a stale
        // cursor_row_from_top here is what makes erase_footer walk the
        // cursor to the wrong row after /login or any other external
        // terminal hijack.
        self.footer_rows = 0;
        self.last_footer = FooterState::default();
        self.last_footer_rows.clear();
        self.assistant_continuing = false;
        self.assistant_line_buf.clear();
        self.md_state.reset();
        // Drop any throttled payload too — it references a state that
        // the reset just obliterated.
        self.throttle.clear();
        let _ = self.out.flush();
    }

    fn flush_deferred(&mut self) {
        // Called by the event loop on a 20ms tick. Only actually paints
        // if (a) there's a pending InputPrompt/StreamingBox and (b) the
        // throttle window has elapsed — otherwise it's a no-op so the
        // 50fps timer doesn't blast stale payloads.
        if self.throttle.has_pending() && self.throttle.window_elapsed() {
            self.paint_pending_input();
        }
    }

    fn on_resize(&mut self, _cols: u16, _rows: u16) {
        // DECSTBM region boundary depends on `H - footer_rows`. After
        // a resize `H` changed but our cached scroll_region still
        // targets the old bottom, so subsequent body writes would
        // scroll in the wrong region (or fall outside it entirely).
        // Force a re-sync + full footer repaint. `last_footer_rows`
        // cache gets invalidated implicitly by size_mismatch detection
        // inside `emit_footer_absolute`.
        if !self.caps.scroll_region {
            return;
        }
        // Drop the old region so `sync_scroll_region` re-issues.
        self.scroll_region = None;
        // Force a full footer repaint against the new dimensions.
        // The cache clear ensures every row is re-emitted even if its
        // bytes happen to match (row positions shift with new H).
        self.last_footer_rows.clear();
        if self.footer_rows > 0 {
            self.draw_footer_here();
            let _ = self.out.flush();
        }
    }

    fn clear_screen(&mut self) {
        // Physical-only: wipe the terminal without invalidating the
        // cached footer/stream state. The very next render call will
        // re-erase (no-op on a blank screen) + re-draw the footer, so
        // the cache stays coherent with what we emit.
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        let _ = self.out.flush();
    }

    fn suspend_for_external(&mut self) {
        // Gate on caps so tests / pipe mode / dumb terminals don't try
        // to toggle modes they never entered. `shutdown` handles the
        // final `\r\n` + flush so the external child starts on a clean
        // line. `shutdown` also clears the DECSTBM region, which is
        // critical — OAuth/browser/etc. sub-processes inherit the
        // terminal and must see a full-height scroll area.
        if self.caps.bracketed_paste {
            let _ = execute!(self.out, DisableBracketedPaste);
        }
        if self.caps.raw_mode {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        self.shutdown();
    }

    fn resume_from_external(&mut self) {
        if self.caps.raw_mode {
            let _ = crossterm::terminal::enable_raw_mode();
        }
        if self.caps.bracketed_paste {
            let _ = execute!(self.out, EnableBracketedPaste);
        }
        // Force-clear terminal-side DECSTBM + autowrap state BEFORE
        // calling `reset()`. Rationale: `shutdown()` (called during
        // suspend_for_external) already set `self.scroll_region = None`,
        // so the `clear_scroll_region` inside `reset()` short-circuits
        // and does NOT re-emit `\x1b[r`. If the OAuth child process
        // touched the scroll region itself (browser CLIs, shell rc
        // files that run tput, scripts that set margins), the terminal
        // is now in a scroll region we don't know about. The next body
        // write would scroll inside that unknown region and leave
        // ghost rule characters where they shouldn't be.
        //
        // Unconditionally emitting `\x1b[r\x1b[?7h` is a cheap (8 bytes,
        // no side effects if already clear) way to pin the terminal to
        // a known state — scroll region fully cleared, autowrap on —
        // before `reset()` moves the cursor and redraws.
        if self.caps.scroll_region {
            let _ = self.out.write_all(b"\x1b[r\x1b[?7h");
            let _ = self.out.flush();
            // Sync our own tracker with what we just told the terminal.
            self.scroll_region = None;
        }
        // Wipe the screen + forget all caches so the next render rebuilds
        // against a known (row 1, col 1) anchor.
        self.reset();
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
        // CC-style footer: top/bottom horizontal rules (no corners or sides).
        // Minimum 4 consecutive ─ characters confirms a rule was drawn,
        // distinguishing it from ─ that might appear inside content.
        assert!(s.contains("────"));
        // Corners `╭╰╮╯` and side `│` must NOT appear — regression guard
        // against the old full-box rendering coming back.
        assert!(!s.contains("╭"));
        assert!(!s.contains("╰"));
        assert!(!s.contains("│"));
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

    /// Writer that just tallies byte counts — lets us sample
    /// `bytes_written` mid-session without borrowing conflicts.
    struct CountingSink {
        bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }
    impl std::io::Write for CountingSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.bytes.fetch_add(b.len() as u64, std::sync::atomic::Ordering::Relaxed);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn new_counting_renderer() -> (AnsiRenderer<CountingSink>, std::sync::Arc<std::sync::atomic::AtomicU64>) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sink = CountingSink { bytes: counter.clone() };
        (AnsiRenderer::with_writer(sink, caps_with_color()), counter)
    }

    fn sample(counter: &std::sync::Arc<std::sync::atomic::AtomicU64>) -> u64 {
        counter.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Quantify how many bytes a typical streaming TextDelta costs.
    /// Mirrors the real event-loop pattern: footer is established by a
    /// StreamingBox, then TextDelta arrives and is followed by a
    /// StreamingBox redraw (spinner + box below). Each such cycle is
    /// what Mac Terminal.app's GUI pipeline has to eat per ~20ms during
    /// streaming — if it's >500 bytes per delta, we're saturating the
    /// terminal's render queue and user perceives post-stream input lag.
    #[test]
    fn streaming_text_delta_byte_cost() {
        let (mut r, counter) = new_counting_renderer();
        let status = super::super::StatusLine {
            model: "glm-4.5".into(),
            cwd: "~/project/atomcode".into(),
            total_tokens: 0,
            hint: None,
        };

        // Establish initial streaming footer.
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Thinking".into(),
            status: status.clone(),
            menu: None,
        });
        r.flush();
        let baseline = sample(&counter);

        // Simulate a single TextDelta + its follow-up StreamingBox redraw.
        r.render(UiLine::AssistantText("这是一段 streaming 的文字。\n".into()));
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠙",
            label: "Thinking".into(),
            status: status.clone(),
            menu: None,
        });
        r.flush();
        let per_delta = sample(&counter) - baseline;

        // 20 more deltas for steady-state average.
        let before_burst = sample(&counter);
        for i in 0..20 {
            r.render(UiLine::AssistantText(format!("行 {}\n", i)));
            r.render(UiLine::StreamingBox {
                buf: String::new(),
                cursor_byte: 0,
                frame: "⠹",
                label: "Thinking".into(),
                status: status.clone(),
                menu: None,
            });
        }
        r.flush();
        let avg_per_delta = (sample(&counter) - before_burst) / 20;

        eprintln!(
            "[BYTE TEST] streaming: first delta = {} B, avg over 20 = {} B",
            per_delta, avg_per_delta
        );
        // Diagnostic only — no assertion. We're measuring current behaviour
        // to decide whether a batch-redraw optimisation is warranted.
    }

    #[test]
    fn keystroke_byte_cost_steady_state() {
        let (mut r, counter) = new_counting_renderer();
        let status = super::super::StatusLine {
            model: "glm-4.5".into(),
            cwd: "~/project/atomcode".into(),
            total_tokens: 42,
            hint: None,
        };

        // Warm the footer cache with one InputPrompt.
        r.render(UiLine::InputPrompt {
            buf: "h".into(),
            cursor_byte: 1,
            menu: None,
            status: status.clone(),
        });
        r.flush();
        let before = sample(&counter);

        // 10 keystrokes — only the input buf grows.
        for i in 1..=10 {
            let s: String = "h".repeat(i + 1);
            r.render(UiLine::InputPrompt {
                buf: s.clone(),
                cursor_byte: s.len(),
                menu: None,
                status: status.clone(),
            });
        }
        r.flush();
        let avg_per_keystroke = (sample(&counter) - before) / 10;
        eprintln!(
            "[BYTE TEST] keystroke steady-state avg = {} B",
            avg_per_keystroke
        );
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
