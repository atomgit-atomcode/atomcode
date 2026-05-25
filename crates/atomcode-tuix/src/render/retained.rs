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

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;

use super::cell::{push_str_cells, serialize_row, Cell, CellStyle};
use super::screen::Screen;
use super::theme::{role, Role};
use super::{MenuPayload, Renderer, StatusLine, UiLine};
use crate::i18n::{t, Msg};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;
use crossterm::style::Color;

const PAD_COL: usize = 2;

/// Max body_lines kept in the in-app scrollback buffer (matches alt-screen).
/// Bounded so memory doesn't grow without limit on long sessions.
pub const MAX_SCROLLBACK_ROWS: usize = 5000;

/// Render context usage as `12.3k / 131k tok` when both used and window
/// are known, or `12.3k tok` when only the used count is known (provider
/// hasn't reported its window yet, e.g. pre-config or fallback).
fn format_ctx_usage(used: usize, window: usize) -> String {
    let used_label = if used < 1000 {
        format!("{}", used)
    } else {
        format!("{:.1}k", (used as f64) / 1000.0)
    };
    if window == 0 {
        format!("{} tok", used_label)
    } else {
        let window_label = if window < 1000 {
            format!("{}", window)
        } else if window % 1000 == 0 {
            format!("{}k", window / 1000)
        } else {
            format!("{:.0}k", (window as f64) / 1000.0)
        };
        format!("{}/{} tok", used_label, window_label)
    }
}

// ── Markdown → Cell parser ─────────────────────────────────────────
//
// `crate::markdown::render_line` returns an ANSI-tinted string: the
// markdown text with SGR escapes embedded (e.g. `**bold**` →
// `\x1b[1mbold\x1b[22m`, `` `code` `` → `\x1b[97mcode\x1b[39m`).
// AnsiRenderer wrote those bytes straight to stdout. Retained mode
// works on `Cell`s, so we parse the ANSI string back into a stream
// of cells carrying their computed style. Minimal parser — handles
// only the SGR vocabulary our markdown crate emits:
//
//   1     bold on
//   22    bold off
//   3     italic on   (folded — CellStyle has no italic bit, so
//                      italic text renders plain. Same visual loss
//                      we'd have without markdown support at all;
//                      acceptable for Phase 6.)
//   23    italic off
//   7     reverse on
//   27    reverse off
//   39    fg default
//   90    fg DarkGrey (borders / soft headings)
//   97    fg White (inline code / code blocks — bright white)
//   0     reset everything
//
// Other SGR params (RGB, 256-color, italic, underline) are silently
// ignored — the glyph still renders with the current accumulated
// style. CSI sequences with a non-`m` final byte are skipped whole.

/// Parse an ANSI-tinted markdown string into one or more cell
/// lines, split on `\n`. Wide glyphs get one real cell + N-1
/// `Cell::continuation()` cells so `cell_index == terminal_column`
/// stays true.
fn parse_markdown_to_cells(s: &str) -> Vec<Vec<Cell>> {
    let mut lines: Vec<Vec<Cell>> = vec![Vec::new()];
    let mut style = CellStyle::default();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                let mut params = String::new();
                while let Some(&p) = chars.peek() {
                    chars.next();
                    if p.is_ascii_alphabetic() || p == '~' {
                        if p == 'm' {
                            apply_sgr(&params, &mut style);
                        }
                        break;
                    }
                    params.push(p);
                }
            }
            continue;
        }
        if c == '\n' {
            lines.push(Vec::new());
            continue;
        }
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        if w == 0 {
            continue;
        }
        lines.last_mut().unwrap().push(Cell {
            ch: c,
            style: style.clone(),
            width: w as u8,
        });
        for _ in 1..w {
            lines.last_mut().unwrap().push(Cell::continuation());
        }
    }
    lines
}

/// Clip a cell row to at most `max_cols` display columns. Drops
/// trailing cells (including their continuation cells) so the total
/// `cell.width` sum of the returned row is ≤ `max_cols`. A wide
/// glyph that straddles `max_cols` is dropped whole — we never emit
/// the left half without its continuation, which would leak into
/// the next line on real terminals once auto-wrap kicks in.
///
/// Used on the resize path to make cached `body_lines` (built for
/// the OLD screen width) safe to re-emit against a narrower new
/// terminal. Without this, `serialize_row` would emit glyphs past
/// the right edge; the terminal's own auto-wrap then spills them
/// into the next row — which is the footer strip or a phantom body
/// row — producing the "everything shifted by one column and the
/// footer has garbage in it" symptom after a resize-smaller drag.
fn clip_cells_to_width(cells: &[Cell], max_cols: usize) -> Vec<Cell> {
    if max_cols == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(cells.len().min(max_cols));
    let mut used = 0usize;
    for cell in cells {
        let w = cell.width as usize;
        if w > 0 && used + w > max_cols {
            break;
        }
        out.push(cell.clone());
        used += w;
    }
    out
}

/// Cell-based wrap: splits a cell sequence into chunks whose sum
/// of `cell.width` stays ≤ `max_cols`. Continuation cells (width 0)
/// travel with their preceding real cell — the combined "grapheme"
/// never splits mid-wide-glyph.
fn wrap_cells_to_width(cells: &[Cell], max_cols: usize) -> Vec<Vec<Cell>> {
    if max_cols == 0 || cells.is_empty() {
        return vec![cells.to_vec()];
    }
    let mut chunks: Vec<Vec<Cell>> = vec![Vec::new()];
    let mut cur_width = 0usize;
    for cell in cells {
        let w = cell.width as usize;
        if w > 0 && cur_width + w > max_cols && !chunks.last().unwrap().is_empty() {
            chunks.push(Vec::new());
            cur_width = 0;
        }
        chunks.last_mut().unwrap().push(cell.clone());
        cur_width += w;
    }
    chunks
}

fn apply_sgr(params: &str, style: &mut CellStyle) {
    // `\x1b[m` (empty params) is treated as SGR 0 per ECMA-48.
    let parts: Vec<&str> = if params.is_empty() {
        vec!["0"]
    } else {
        params.split(';').collect()
    };
    let mut i = 0;
    while i < parts.len() {
        let part = parts[i];
        match part.parse::<u32>().ok() {
            Some(0) => *style = CellStyle::default(),
            Some(1) => style.bold = true,
            Some(2) => style.faint = true,
            Some(22) => {
                // ECMA-48 22 = normal intensity — clears both bold AND
                // faint as a pair. There is no per-attribute toggle for
                // faint, so bold→off and faint→off both route through 22.
                style.bold = false;
                style.faint = false;
            }
            // Italic (3/23) — no CellStyle bit; text renders plain.
            Some(3) | Some(23) => {}
            Some(7) => style.reverse = true,
            Some(27) => style.reverse = false,
            Some(39) => style.fg = None,
            Some(90) => style.fg = Some(Color::DarkGrey),
            Some(91) => style.fg = Some(Color::Red),
            Some(92) => style.fg = Some(Color::Green),
            Some(93) => style.fg = Some(Color::Yellow),
            Some(94) => style.fg = Some(Color::Blue),
            Some(95) => style.fg = Some(Color::Magenta),
            Some(96) => style.fg = Some(Color::Cyan),
            Some(97) => style.fg = Some(Color::White),
            // 38;2;R;G;B — truecolor foreground. Markdown emits this
            // for inline code / code blocks / headings so the colour
            // survives terminal palette remapping (bright-XX colours
            // get re-tinted by themes; truecolor RGB does not).
            // Consume 4 extra tokens (`2`, R, G, B) on success.
            Some(38) => {
                if parts.get(i + 1).copied() == Some("2") {
                    if let (Some(r), Some(g), Some(b)) = (
                        parts.get(i + 2).and_then(|s| s.parse::<u8>().ok()),
                        parts.get(i + 3).and_then(|s| s.parse::<u8>().ok()),
                        parts.get(i + 4).and_then(|s| s.parse::<u8>().ok()),
                    ) {
                        style.fg = Some(Color::Rgb { r, g, b });
                        i += 4;
                    }
                }
                // 38;5;N (256-colour) and other 38 sub-formats fall
                // through silently — markdown doesn't emit them.
            }
            _ => {
                // Other ANSI colours (30-37, 91-96, bg, underline)
                // silently ignored — markdown doesn't emit them.
            }
        }
        i += 1;
    }
}

pub struct RetainedRenderer<W: Write + Send> {
    out: W,
    caps: TerminalCaps,
    screen: Screen,
    // ── widget state ──
    input_buf: String,
    input_cursor_byte: usize,
    menu: Option<MenuPayload>,
    status: StatusLine,
    /// Marker numbers (`N`) that should render as `└ [Image #N]`
    /// preview rows directly under the input box. Pre-computed by
    /// `event_loop::compute_input_attachments` (intersect of buffer
    /// `[Image #N]` markers with `pending_image_markers` +
    /// `pending_recalled_attachments`), so we draw a row only when
    /// the buffer text really maps to image bytes ready to ship —
    /// not for literal `[Image #N]` strings the user typed by hand.
    /// Always rendered in `Role::Muted`, mirroring the post-submit
    /// `UiLine::ImageAttachment` echo style so the visual contract
    /// pre- and post-submit reads identically.
    input_attachments: Vec<usize>,
    // ── body history ──
    /// Pre-wrapped body rows, oldest first. Trimmed when exceeds
    /// 2× screen height. Symbol-bearing rows (`❯`, `▸`, `▶`, `⎿`)
    /// are flush-left at col 0; plain text rows (assistant prose,
    /// errors, cancelled, cmd output, diff, turn separator) carry a
    /// `PAD_COL` indent. `paint_body` just `draw_row`s the last N
    /// directly.
    body_lines: Vec<Vec<Cell>>,
    /// Message boundary markers for "jump to prev/next message" navigation.
    /// Tracks which line_idx marks the start of a User / Assistant / ToolCall / ToolResult message.
    message_marks: Vec<crate::render::MessageMark>,
    /// True if the last mark pushed was `MarkKind::Assistant`. Used to de-duplicate
    /// marks for multi-chunk `UiLine::AssistantText` streams — only the first chunk
    /// of a turn gets a new mark; subsequent chunks within the same assistant turn are silent.
    /// Cleared whenever a User / ToolCall / ToolCallInFlight / TurnSeparator fires.
    last_mark_was_assistant: bool,
    /// True iff user has scrolled away from the tail. While true, body
    /// emit suppresses terminal writes and paint_body redraws from
    /// body_lines[viewport_top..] via CUP+EL instead of DECSTBM \n.
    view_mode: bool,
    /// Top body_lines index visible at body region top, when view_mode = true.
    viewport_top: usize,
    /// True iff viewport_top >= max_top (auto-tail). Drives view_mode entry/exit.
    sticky_bottom: bool,
    /// Line-buffer for streaming assistant text — chunks accumulate
    /// here until a `\n` boundary, at which point the completed
    /// physical line is appended to `body_lines`.
    assistant_line_buf: String,
    /// Markdown parser state (code-block tracking, table row
    /// buffering) passed to `crate::markdown::render_line` on each
    /// completed assistant line.
    md_state: crate::markdown::MdState,
    // ── Phase 5: frame coalescing ──
    /// True when widget state has changed since the last frame
    /// emit. `render()` flips this to true instead of painting
    /// immediately; `flush_deferred()` (called every 5ms by the
    /// event loop tick) checks this and does the paint+emit at
    /// most once per tick. An IME burst of 40 keystrokes in 1ms
    /// thus produces ONE frame instead of 40 — the difference
    /// between 40 Mac Terminal repaints and 1.
    dirty: bool,
    /// Footer row count at the last successful emit. When footer
    /// geometry changes (wrap, menu open/close), absolute row
    /// positions of the internal layout stay the same for some
    /// rows but shift for others — and on Mac Terminal.app we've
    /// observed the "rule" rows occasionally rendering as
    /// half-width after such a transition, even though
    /// `cells[row_57]` holds the full 209 dashes. Rather than
    /// chase the terminal-side glitch, we invalidate prev_cells
    /// on geometry change so the next paint emits every row
    /// full-frame, guaranteeing the terminal re-processes the
    /// rule regardless of diff skip.
    last_painted_footer_rows: usize,
    /// Bottom row (1-indexed) of the currently-set DECSTBM region.
    /// `None` means "no region set" (terminal default = full screen).
    /// Updated by `ensure_scroll_region()` before any body/footer
    /// paint so `\n` in the body-emit path only scrolls body rows,
    /// leaving the footer strip below untouched.
    scroll_region_bottom: Option<u16>,
    /// Set by `pop_approval_prompt` so the immediately-following
    /// body-line emit overwrites the approval row in place instead of
    /// scrolling the region up one row. Without this, the ToolResult
    /// that follows Y/A/N would push the ▸ ToolCall row off to make
    /// space for itself, leaving a blank gap between `▸ Tool(detail)`
    /// and `⎿ result`.
    /// Number of upcoming `push_body_row` calls that should overwrite in
    /// place instead of scrolling the body region. Set by
    /// `pop_approval_prompt` when the popped approval block occupied
    /// more than one terminal row — each skipped scroll closes one row
    /// of the gap between the last content row and body_bottom.
    /// Decremented on every `emit_body_line_inner` call.
    skip_body_scroll_count: u16,
    /// Cached semantic welcome payload so resize can rebuild the
    /// startup banner for the new terminal width.
    welcome_banner: Option<(String, String)>,
    /// Number of rows occupied by the welcome banner prefix in
    /// `body_lines`.
    welcome_line_count: usize,
    /// True when `body_lines.last()` is a LIVE spinner row (the
    /// emoji/label pair emitted by `UiLine::Spinner` /
    /// `UiLine::StreamingBox`). A live row gets in-place re-emitted
    /// on each subsequent spinner tick so body_lines doesn't grow
    /// one entry per frame. Any non-spinner body push finalises
    /// the row (flag flips to false) so the last animation frame
    /// stays frozen as a historical paragraph header.
    live_spinner_active: bool,
    /// When `Some`, the live row at body_bottom is the animated
    /// in-flight tool-call line (`<frame> Bash(cmd)`), not the generic
    /// spinner. The Spinner / StreamingBox tick handlers consult this:
    /// if Some they build a tool-call row with the new frame as icon;
    /// if None they build the generic `<frame> Pondering…` spinner row.
    /// Cleared by `ToolCallCommit`, which freezes the row to a static
    /// `▸` icon (no longer live) so the next push_body_row appends
    /// cleanly below it and the spinner can resume on the next tick.
    /// (call_id, name, detail).
    inflight_tool: Option<(String, String, String)>,
    /// Number of body lines occupied by the multi-line wrapped in-flight
    /// tool call (rendered via `render_inflight_tool`). Used to replace
    /// those lines on each spinner tick and to clean up on commit.
    inflight_tool_rows: usize,
    /// Active multi-row "live group" — the tail of `body_lines` is one
    /// header + N child rows for a parallel tool batch. Subsequent
    /// `UiLine::ToolGroupChildUpdate` events resolve `call_id` →
    /// `body_lines` index via the `child_indices` map and CUP+rewrite
    /// in place, mirroring CC's `Read 4 files` block where each row
    /// lights up `✓` as its result lands. Any external `push_body_row`
    /// freezes the group (flag taken: subsequent updates fall back to
    /// no-op since the group rows are no longer at the bottom and may
    /// have scrolled out of the visible body strip).
    live_group: Option<LiveGroup>,
}

/// Tracking state for an active multi-row live group. Populated by
/// `UiLine::ToolGroupRender`, consulted by `UiLine::ToolGroupChildUpdate`,
/// cleared by any unrelated `push_body_row`.
#[derive(Debug, Clone)]
struct LiveGroup {
    batch_id: String,
    /// Index of the header row in `body_lines`. Reserved for a
    /// follow-up `ToolGroupHeaderUpdate` variant that appends the
    /// `· N/M ok · Xs wall` summary in-place on batch completion
    /// instead of pushing a separate row.
    #[allow(dead_code)]
    header_idx: usize,
    /// `call_id` → index into `body_lines` for each child row. Indices
    /// are absolute; they remain valid as long as no rows are drained
    /// from the front of `body_lines` while the group is live.
    child_indices: std::collections::HashMap<String, usize>,
}

impl RetainedRenderer<BufWriter<Stdout>> {
    pub fn new(caps: TerminalCaps) -> Self {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        Self::with_writer(BufWriter::new(std::io::stdout()), caps, w, h)
    }
}

impl<W: Write + Send> RetainedRenderer<W> {
    pub fn with_writer(mut out: W, caps: TerminalCaps, w: u16, h: u16) -> Self {
        // Clear scrollback buffer so previous terminal content (e.g. git log)
        // doesn't remain visible above the atomcode viewport and mix with
        // the atomcode session transcript. `\x1b[3J` only affects scrollback;
        // it does not touch the visible screen rows.
        let _ = out.write_all(b"\x1b[3J");
        let _ = out.flush();
        Self {
            out,
            caps,
            screen: Screen::new(w, h),
            input_buf: String::new(),
            input_cursor_byte: 0,
            menu: None,
            status: StatusLine::default(),
            input_attachments: Vec::new(),
            body_lines: Vec::new(),
            message_marks: Vec::new(),
            last_mark_was_assistant: false,
            view_mode: false,
            viewport_top: 0,
            sticky_bottom: true,
            assistant_line_buf: String::new(),
            md_state: crate::markdown::MdState::new(),
            dirty: false,
            last_painted_footer_rows: 0,
            scroll_region_bottom: None,
            skip_body_scroll_count: 0,
            welcome_banner: None,
            welcome_line_count: 0,
            live_spinner_active: false,
            inflight_tool: None,
            inflight_tool_rows: 0,
            live_group: None,
        }
    }

    // ── Widget row builders (Cell-valued, no direct I/O) ──
    //
    // These are structurally identical to the ones in
    // `render/ansi.rs` — when Phase 6 deletes AnsiRenderer, the
    // duplication collapses (retained becomes the only owner).
    // Keeping them verbatim here for Phase 3 means we don't have
    // to refactor two renderers at once: the visual output is
    // byte-exact against what AnsiRenderer produced in the same
    // situation, giving the dual-track byte-cost tests a fair
    // comparison.

    fn style_for(&self, r: Role) -> CellStyle {
        CellStyle {
            fg: role(self.caps, r),
            bold: false,
            reverse: false,
            faint: false,
        }
    }

    fn style_bold(&self, r: Role) -> CellStyle {
        CellStyle {
            fg: role(self.caps, r),
            bold: true,
            reverse: false,
            faint: false,
        }
    }

    /// Theme-aware muting via SGR 2 (faint). Renders the role's fg
    /// at ~50% intensity so secondary text reads as "subordinate"
    /// without picking a fixed gray that may collide with the user's
    /// terminal palette. Pair with `Role::Secondary` (no fg) to dim
    /// the terminal default fg — the canonical "muted hint" look that
    /// adapts across light/dark themes.
    fn style_faint(&self, r: Role) -> CellStyle {
        CellStyle {
            fg: role(self.caps, r),
            bold: false,
            reverse: false,
            faint: true,
        }
    }

    /// Build the cells for a spinner body row: `<frame> <label>`,
    /// flush-left at col 0 (no PAD_COL indent) so the frame glyph
    /// aligns with `❯` user echoes and `▸` tool calls in the same
    /// column. Used by the live spinner path to paint / re-paint
    /// the "in-progress" row each tick.
    fn build_spinner_body_row(&self, frame: &str, label: &str) -> Vec<Cell> {
        let mut row = Vec::new();
        let frame_style = self.style_for(Role::Brand);
        push_str_cells(&mut row, frame, &frame_style);
        push_str_cells(&mut row, " ", &CellStyle::default());
        let label_style = self.style_bold(Role::Secondary);
        push_str_cells(&mut row, &scrub_controls(label), &label_style);
        row
    }

    /// Render (or re-render) the in-flight tool-call body text using
    /// `icon` as the prefix, with proper multi-line wrapping via
    /// `push_body_prefixed`. Removes any previously rendered inflight
    /// tool lines from `body_lines` first so the spinner animation
    /// replaces in-place rather than accumulating rows.
    fn render_inflight_tool(&mut self, icon: &str, name: &str, detail: &str, meta: &str) {
        // Spinner ticks fire at ~80ms cadence and re-call this fn with a
        // new icon glyph each time. The OLD implementation truncated
        // `body_lines` and called `push_body_prefixed` → `push_body_row`
        // → `emit_body_line_inner` which uses `\n` to scroll new content
        // into the DECSTBM body region. The model-state truncation hid
        // the leak from the existing in-process test (`body_lines.len()`
        // stayed flat) but the *terminal output* path scrolled a fresh
        // copy of the inflight row IN every tick. After ~30s of cargo
        // build, the user's scrollback held 30+ identical
        // `▸ Bash(... cargo build ...)` rows even though the model only
        // emitted ONE call (verified via datalog).
        //
        // Fix: when re-rendering on top of a prior inflight render with
        // matching row count (the 99% case — only the icon glyph
        // changes, all 1-cell-wide), bypass `push_body_row` entirely.
        // Position the cursor at each previously-rendered row, erase
        // the line, write the new cells. No `\n`, no scroll, no
        // scrollback growth — same approach `push_or_update_live_spinner`
        // already uses for the ordinary spinner row.
        //
        // Fallback (`prev_rows == 0`, or row count differs because
        // the terminal was resized between ticks) keeps the original
        // scroll-push semantics so layout still settles correctly; the
        // one-frame scrollback ghost on a resize is acceptable since
        // it doesn't accumulate across ticks.
        let safe_name = scrub_controls(name);
        let safe_detail = scrub_controls(detail);
        let body_str = if safe_detail.is_empty() {
            safe_name
        } else {
            format!("{}({})", safe_name, safe_detail)
        };
        // Safety cap: prevent degenerate bodies (e.g. multi-KB bash
        // commands) from producing hundreds of terminal lines.
        // This is a rendering safeguard only — the actual command
        // execution uses the original, untruncated arguments.
        let body_str = truncate_body_str(&body_str, 500);
        // Append the spinner meta suffix (e.g. ` · 12s` or
        // ` · 12s · 2 queued`) so the user has a time anchor while a
        // long-running tool (cargo install, big test suite, etc.)
        // executes. Without it the inflight row only shows
        // `<spinner> Bash(cmd)` — no elapsed indicator — and looks
        // indistinguishable from "stuck" once the user has been
        // waiting >30s. `meta` carries its own leading ` · ` separator
        // (or is empty); same single body style as the rest of the
        // row, matching `build_spinner_body_row`'s convention where
        // the suffix shares the label colour.
        let body_str = if meta.is_empty() {
            body_str
        } else {
            format!("{}{}", body_str, meta)
        };
        let prefix = format!("{} ", icon);
        let prefix_style = self.style_for(Role::Muted);
        let body_style = self.style_bold(Role::ToolName);
        let new_rows = self.build_prefixed_rows(&prefix, &prefix_style, &body_str, &body_style);

        let prev_rows = self.inflight_tool_rows;
        let n = new_rows.len();
        if n == 0 {
            // Nothing to render (zero-width terminal etc.) — drop any
            // prior inflight rows so state stays consistent.
            let remove = prev_rows.min(self.body_lines.len());
            self.body_lines.truncate(self.body_lines.len() - remove);
            self.inflight_tool_rows = 0;
            return;
        }

        self.ensure_scroll_region();
        let bottom = self.body_bottom_row();
        let inplace_ok = prev_rows > 0 && n == prev_rows && bottom >= n as u16;
        if inplace_ok {
            // In-place rewrite: the prior render's terminal rows are at
            // (bottom - n + 1 ..= bottom). Update model state by
            // swapping the trailing slice; then walk each terminal row
            // with a position + erase + write triple.
            let keep = self.body_lines.len().saturating_sub(prev_rows);
            self.body_lines.truncate(keep);
            let first = bottom - n as u16 + 1;
            for (i, row) in new_rows.iter().enumerate() {
                let r = first + i as u16;
                let seq = format!("\x1b[{};1H\x1b[2K", r);
                let _ = self.out.write_all(seq.as_bytes());
                let bytes = serialize_row(row);
                let _ = self.out.write_all(&bytes);
                self.body_lines.push(row.clone());
            }
        } else {
            // First render or row-count mismatch — fall back to scroll-push.
            // Drop any prior inflight rows from model state; push new rows
            // via the standard path so DECSTBM scrolling lands them at the
            // bottom of the body region.
            let remove = prev_rows.min(self.body_lines.len());
            self.body_lines.truncate(self.body_lines.len() - remove);
            for row in new_rows {
                self.push_body_row(row);
            }
        }
        self.inflight_tool_rows = n;
    }

    /// Pad a partially-built row with blank default-style cells until it
    /// spans `target_w` display columns. Footer rows MUST be padded before
    /// `draw_row` — otherwise stale body cells (welcome banner /provider
    /// hint, previous turn text scrolled up through DECSTBM, etc.) bleed
    /// through past the footer text on both iTerm2 and Terminal.app.
    /// Our screen cell model doesn't track bytes written via
    /// `emit_body_line_inner` (direct stdout), so the diff can't detect
    /// the staleness and won't emit erase bytes unless we write explicit
    /// blanks here.
    fn pad_row_to_width(row: &mut Vec<Cell>, target_w: usize) {
        let cur: usize = row.iter().map(|c| c.width as usize).sum();
        if cur >= target_w {
            return;
        }
        let blank = Cell {
            ch: ' ',
            style: CellStyle::default(),
            width: 1,
        };
        for _ in cur..target_w {
            row.push(blank.clone());
        }
    }

    fn build_rule_row(&self, rule_width: usize) -> Vec<Cell> {
        let mut row = Vec::with_capacity(rule_width);
        let border = self.style_for(Role::Border);
        for _ in 0..rule_width {
            row.push(Cell {
                ch: '─',
                style: border.clone(),
                width: 1,
            });
        }
        row
    }

    /// Top-rule variant that may overlay a session-name pill on the
    /// right side. Mirrors the alt-screen renderer's top-rule overlay
    /// so both render paths show CC-style per-conversation badge. The
    /// bot_rule keeps using `build_rule_row` (no badge there).
    ///
    /// Budget mirrors `alt_screen::paint_footer`:
    ///   right_margin  = 2 cells
    ///   pill_padding  = 2 cells (one space each side of the name)
    ///   min_rule_left = 8 cells (keep some ─ on the left so the box
    ///                  still reads as bordered)
    /// Name truncated with `…` when display_width exceeds budget; if
    /// the rule is too narrow for chrome + 1 cell, the badge is
    /// skipped entirely and a plain rule is returned.
    fn build_top_rule_with_badge(
        &self,
        rule_width: usize,
        session_name: Option<&str>,
    ) -> Vec<Cell> {
        let mut row = self.build_rule_row(rule_width);
        let Some(name) = session_name else {
            return row;
        };
        if name.is_empty() {
            return row;
        }
        const RIGHT_MARGIN: usize = 2;
        const PILL_PADDING: usize = 2;
        const MIN_RULE_LEFT: usize = 8;
        let chrome = RIGHT_MARGIN + PILL_PADDING + MIN_RULE_LEFT;
        if rule_width <= chrome {
            return row;
        }
        let max_name_w = rule_width - chrome;
        let name_w = crate::width::display_width(name);
        let name_for_pill = if name_w <= max_name_w {
            name.to_string()
        } else if max_name_w <= 1 {
            "…".to_string()
        } else {
            let truncated = crate::width::truncate_to_width(name, max_name_w - 1);
            format!("{}…", truncated)
        };
        let pill_text = format!(" {} ", name_for_pill);
        let pill_w = crate::width::display_width(&pill_text);
        // Pill ends RIGHT_MARGIN cells from the right edge. Pill
        // start cell index (0-indexed) = rule_width - RIGHT_MARGIN -
        // pill_w. Saturating sub guards against arithmetic underflow
        // if a future budget tweak shrinks the chrome below right_margin.
        let pill_start = rule_width.saturating_sub(RIGHT_MARGIN + pill_w);
        let pill_style = CellStyle {
            fg: role(self.caps, Role::Border),
            bold: false,
            reverse: true,
            faint: false,
        };
        let mut overlay_cells = Vec::new();
        push_str_cells(&mut overlay_cells, &pill_text, &pill_style);
        // Splice into `row` starting at pill_start. push_str_cells
        // emits continuation cells (width 0) for wide glyphs so the
        // overlay length already matches `pill_w` terminal columns;
        // a straight overwrite preserves cell_index == column.
        for (i, cell) in overlay_cells.into_iter().enumerate() {
            let idx = pill_start + i;
            if idx >= row.len() {
                break;
            }
            row[idx] = cell;
        }
        row
    }

    fn build_middle_row(&self, line: &str, is_first: bool) -> Vec<Cell> {
        let mut row = Vec::new();
        let pad = CellStyle::default();
        if is_first {
            let accent = self.style_for(Role::Accent);
            push_str_cells(&mut row, self.caps.prompt_chevron(), &accent);
        } else {
            push_str_cells(&mut row, "  ", &pad);
        }
        push_str_cells(&mut row, line, &pad);
        row
    }

    fn build_menu_row(
        &self,
        name: &str,
        desc: &str,
        selected: bool,
        rule_width: usize,
        kind: super::MenuKind,
    ) -> Vec<Cell> {
        let mut row = Vec::new();
        // Both menu kinds hug the left edge — content prefixes (`▸ /`
        // or `+ `) carry the visual structure. The previous PAD_COL
        // outer indent compounded with inner format-string padding to
        // push the `▸` arrow 4 columns right of the rule edge, which
        // read as a wonky margin against the flush-left rule.
        let content = match kind {
            super::MenuKind::SlashCommand => {
                // Pad by DISPLAY width, not char count: `/设为默认`
                // (5 chars, 9 cells) needs the same description
                // start column as `/添加` (3 chars, 5 cells), so
                // `{:<12}`'s char-count padding leaves CJK rows
                // pushed two cells to the right of their ASCII
                // neighbours. UnicodeWidthStr knows CJK glyphs are
                // 2 cells; compute and append spaces explicitly.
                let name_width = unicode_width::UnicodeWidthStr::width(name);
                let pad = 12usize.saturating_sub(name_width);
                let padded = format!("{}{}", name, " ".repeat(pad));
                if selected {
                    format!("▸ /{}  {}", padded, desc)
                } else {
                    format!("  /{}  {}", padded, desc)
                }
            }
            super::MenuKind::AtMention => {
                // `+ <path>` for every row; selection is signalled by
                // reverse-video on the row, no extra arrow needed.
                if desc.is_empty() {
                    format!("+ {}", name)
                } else {
                    format!("+ {}  {}", name, desc)
                }
            }
        };

        let style = if selected {
            CellStyle {
                fg: None,
                bold: true,
                reverse: true,
                faint: false,
            }
        } else {
            // Use terminal default fg (Secondary) instead of Muted
            // (SGR 90 / DarkGrey). Several iTerm2 dark presets render
            // bright-black at near-zero contrast against the bg, which
            // makes the entire menu list invisible. Visual hierarchy
            // here comes from the ▸ arrow + reverse-video on the
            // selected row, not from a colour-contrast distinction.
            self.style_for(Role::Secondary)
        };
        push_str_cells(&mut row, &content, &style);

        if selected {
            let content_w = crate::width::display_width(&content);
            let right_pad = rule_width.saturating_sub(content_w);
            for _ in 0..right_pad {
                row.push(Cell {
                    ch: ' ',
                    style: style.clone(),
                    width: 1,
                });
            }
        }
        row
    }

    fn build_status_row(&self, status: &StatusLine, rule_width: usize) -> Vec<Cell> {
        let mut row = Vec::new();
        let pad = CellStyle::default();
        push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);

        // Status row carries load-bearing info (model / cwd / token count)
        // and live hints. Use faint (SGR 2) over the terminal default fg:
        // theme-aware muting that reads as subordinate without picking a
        // fixed gray (DarkGrey collides with several iTerm2 light presets;
        // unmuted default fg made the status row compete with primary
        // body content on dark presets — see screenshot regression).
        let secondary = self.style_faint(Role::Secondary);
        let error = self.style_for(Role::Error);
        let brand = self.style_for(Role::Brand);

        // Mode indicator first — non-default modes (Plan today) prepend
        // a brand-colored badge so the user sees at a glance that file
        // edits / shell are gated. Build (default) is None and adds
        // nothing.
        let mode_badge: Option<String> = status
            .mode_indicator
            .as_ref()
            .map(|s| scrub_controls(s));
        let mode_badge_w = mode_badge
            .as_ref()
            .map(|s| crate::width::display_width(s) + 1) // +1 for the trailing space separator
            .unwrap_or(0);

        // Hint right-alignment math must reserve space for the mode badge
        // so the badge never collides with the right-aligned hint when the
        // status row is wide.
        let max = rule_width.max(1);
        let left_max = max.saturating_sub(mode_badge_w);

        // Pre-truncate the cwd so that model + ctx_usage still get space
        // on narrow terminals.  Budget for cwd: subtract model width and
        // the " · " separator widths from left_max.  If the cwd alone
        // would eat the entire row, `truncate_path` replaces leading
        // segments with ".../" and keeps only the last segment.
        let model_str = if !status.model.is_empty() {
            scrub_controls(&status.model)
        } else {
            String::new()
        };
        let ctx_str = if status.ctx_used > 0 {
            format_ctx_usage(status.ctx_used, status.ctx_window)
        } else {
            String::new()
        };
        // Widths of the static " · " separators between visible parts.
        let sep_w = if !model_str.is_empty() { 3 } else { 0 }
            + if !ctx_str.is_empty() && (!model_str.is_empty() || !status.cwd.is_empty()) {
                3
            } else {
                0
            };
        let cwd_budget = left_max
            .saturating_sub(crate::width::display_width(&model_str))
            .saturating_sub(crate::width::display_width(&ctx_str))
            .saturating_sub(sep_w);

        let mut parts: Vec<String> = Vec::with_capacity(3);
        if !model_str.is_empty() {
            parts.push(model_str);
        }
        if !status.cwd.is_empty() {
            let cwd_full = scrub_controls(&status.cwd);
            let cwd_display = if cwd_budget > 0 && crate::width::display_width(&cwd_full) > cwd_budget {
                crate::width::truncate_path(&cwd_full, cwd_budget)
            } else if cwd_budget == 0 {
                crate::width::truncate_path(&cwd_full, left_max)
            } else {
                cwd_full
            };
            parts.push(cwd_display);
        }
        if !ctx_str.is_empty() {
            parts.push(ctx_str);
        }
        let left = parts.join(" · ");

        // Helper: emit the badge (with trailing space) then the rest, so
        // the mode indicator is always at column 0 (after PAD_COL) and
        // both hint / no-hint branches share the same prefix.
        let push_badge = |row: &mut Vec<Cell>| {
            if let Some(badge) = &mode_badge {
                push_str_cells(row, badge, &brand);
                push_str_cells(row, " ", &pad);
            }
        };

        if let Some((raw_hint, severity)) = status.hint.as_ref() {
            let hint = scrub_controls(raw_hint);
            let hint_w = crate::width::display_width(&hint);
            let hint_style = match severity {
                crate::render::HintSeverity::Warning => error,
                crate::render::HintSeverity::Info => secondary.clone(),
            };
            if hint_w + 1 < left_max {
                let left_budget = left_max - hint_w - 1;
                let left_truncated = crate::width::truncate_to_width(&left, left_budget);
                let left_w = crate::width::display_width(&left_truncated);
                let pad_w = max - mode_badge_w - left_w - hint_w;
                push_badge(&mut row);
                push_str_cells(&mut row, &left_truncated, &secondary);
                push_str_cells(&mut row, &" ".repeat(pad_w), &pad);
                push_str_cells(&mut row, &hint, &hint_style);
            } else {
                let truncated = crate::width::truncate_to_width(&left, left_max);
                push_badge(&mut row);
                push_str_cells(&mut row, &truncated, &secondary);
            }
        } else {
            let truncated = crate::width::truncate_to_width(&left, left_max);
            push_badge(&mut row);
            push_str_cells(&mut row, &truncated, &secondary);
        }
        row
    }

    /// Paint the full footer into `self.screen`. Layout mirrors
    /// `AnsiRenderer::draw_footer_here_with_prev_cursor`:
    ///
    ///   row 0: spinner (or blank margin)
    ///   row 1: top rule
    ///   rows 2..2+N: middle input lines (N = wrap_with_cursor line count)
    ///   row 2+N: bottom rule
    ///   rows 3+N..3+N+M: menu items (M = 0..4)
    ///   row 3+N+M: status line (if any chrome)
    ///
    /// Total rows = 1 + 1 + N + 1 + M + status_rows (where status is
    /// 0 or 1). `footer_top = screen.height - total_rows`. Cursor
    /// parks at `(footer_top + 2 + cursor_row_in_middle,
    /// PAD_COL + 2 + cursor_col_in_row)` — 1-indexed at emit.
    fn paint_footer(&mut self) {
        let w = self.screen.width() as usize;
        let h = self.screen.height() as usize;
        if h == 0 || w == 0 {
            return;
        }
        // menu/status keep the PAD_COL margin for visual balance; only
        // the input-box rules and middle row go full-width so the box
        // hugs the screen edges (per user request: remove left/right
        // padding for the input box only).
        let rule_width = w.saturating_sub(PAD_COL * 2);
        let input_rule_width = w;
        // "> " prompt prefix is 2 display cols; text fills the rest.
        let text_budget = input_rule_width.saturating_sub(2);

        // Wrap input + locate cursor in wrapped layout.
        let safe = scrub_controls(&self.input_buf);
        let (mut lines, cursor_row_in_middle, cursor_col_in_row) = if text_budget == 0 {
            (vec![String::new()], 0usize, 0usize)
        } else {
            crate::width::wrap_with_cursor(&safe, text_budget, self.input_cursor_byte)
        };
        if lines.is_empty() {
            lines.push(String::new());
        }
        let middle_rows = lines.len();

        // Paginate menu to 4 items in view around `selected`.
        let (menu_items, selected_in_view) = if let Some(m) = self.menu.as_ref() {
            let len = m.items.len();
            if len == 0 {
                (Vec::<(String, String)>::new(), None)
            } else {
                let offset = if len <= 4 {
                    0
                } else if m.selected < 4 {
                    0
                } else {
                    (m.selected + 1)
                        .saturating_sub(4)
                        .min(len.saturating_sub(4))
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

        // Spinner moved to body as a live paragraph row — footer no
        // longer reserves a spinner slot. Footer layout:
        //   top_rule / middle... / bot_rule / menu... / status
        let menu_rows = menu_items.len().min(4);
        // Attachment-preview rows: one `└ [Image #N]` per kept marker,
        // sitting between bot_rule and the menu. The list arrives
        // pre-filtered by `compute_input_attachments` (only markers
        // backed by real bytes survive), so we trust it directly here
        // and don't re-validate against `input_buf`.
        let attachment_rows = self.input_attachments.len();
        let has_status = !self.status.model.is_empty()
            || !self.status.cwd.is_empty()
            || self.status.hint.is_some();
        let status_rows = if has_status { 1 } else { 0 };
        let total_rows = 1 + middle_rows + 1 + attachment_rows + menu_rows + status_rows;
        let footer_top = h.saturating_sub(total_rows);

        // Pre-build every row vector (immutable borrows of self).
        let top_rule = self.build_top_rule_with_badge(
            input_rule_width,
            self.status.session_name.as_deref(),
        );
        let middle_cells: Vec<Vec<Cell>> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| self.build_middle_row(line, i == 0))
            .collect();
        let bot_rule = self.build_rule_row(input_rule_width);
        let status_clone = self.status.clone();
        let status_cells = if has_status {
            Some(self.build_status_row(&status_clone, rule_width))
        } else {
            None
        };
        let menu_kind = self
            .menu
            .as_ref()
            .map(|m| m.kind)
            .unwrap_or_default();
        let menu_cells: Vec<Vec<Cell>> = menu_items
            .iter()
            .enumerate()
            .map(|(i, (name, desc))| {
                let selected = selected_in_view == Some(i);
                self.build_menu_row(name, desc, selected, rule_width, menu_kind)
            })
            .collect();
        // Attachment rows: `  └ [Image #N]` in muted gray, identical
        // visual treatment to the post-submit `UiLine::ImageAttachment`
        // echo. PAD_COL is the leading 2-space indent every body /
        // footer info row uses; the `└` then sits at col 2, aligned
        // with the `[` of `[Image #N]` in the user input above.
        let attachment_cells: Vec<Vec<Cell>> = self
            .input_attachments
            .iter()
            .map(|n| {
                let mut row = Vec::new();
                let pad = CellStyle::default();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
                let muted = self.style_for(Role::Muted);
                push_str_cells(&mut row, &format!("└ [Image #{}]", n), &muted);
                row
            })
            .collect();

        // Mutate screen (now &mut self). Every footer row is padded to
        // screen width before emit so blank cells overwrite any stale
        // body content still showing from earlier frames (see
        // `pad_row_to_width` for full rationale).
        let mut top_rule = top_rule;
        Self::pad_row_to_width(&mut top_rule, w);
        self.screen.draw_row(footer_top, 0, &top_rule);

        for (i, r) in middle_cells.into_iter().enumerate() {
            let mut padded = r;
            Self::pad_row_to_width(&mut padded, w);
            self.screen.draw_row(footer_top + 1 + i, 0, &padded);
        }

        let bot_rule_row = footer_top + 1 + middle_rows;
        let mut bot_rule = bot_rule;
        Self::pad_row_to_width(&mut bot_rule, w);
        self.screen.draw_row(bot_rule_row, 0, &bot_rule);

        for (i, r) in attachment_cells.into_iter().enumerate() {
            let mut padded = r;
            Self::pad_row_to_width(&mut padded, w);
            self.screen.draw_row(bot_rule_row + 1 + i, 0, &padded);
        }

        let menu_top = bot_rule_row + 1 + attachment_rows;
        for (i, r) in menu_cells.into_iter().enumerate() {
            let mut padded = r;
            Self::pad_row_to_width(&mut padded, w);
            self.screen.draw_row(menu_top + i, 0, &padded);
        }
        if let Some(st) = status_cells {
            let mut padded = st;
            Self::pad_row_to_width(&mut padded, w);
            self.screen.draw_row(menu_top + menu_rows, 0, &padded);
        }

        // Cursor park — 1-indexed, inside middle row at the input cell.
        // Input row is flush-left (no PAD_COL); "> " prefix is 2 cols.
        // Symbol-bearing body rows share this col-0 baseline.
        // Middle row lives at `footer_top + 1 + cursor_row_in_middle`
        // (0-indexed); +1 more to convert to the 1-indexed form the
        // cursor-set helper expects.
        let cursor_abs_row = (footer_top + 1 + cursor_row_in_middle + 1) as u16;
        let cursor_abs_col = (2 + cursor_col_in_row + 1) as u16;
        self.screen.set_cursor(cursor_abs_row, cursor_abs_col);
        // Hide the terminal cursor while EITHER a live spinner OR an
        // inflight-tool row is animating. The inflight branch was added
        // when `render_inflight_tool` switched to direct cursor-position
        // writes (to fix the scrollback-leak bug): those writes leave
        // the real terminal cursor at end-of-row, but `screen` doesn't
        // know that since it bypasses the cell-diff path. Without
        // hiding, the user sees a blinking caret floating at the right
        // edge of the active `▸ Bash(...)` row in addition to the input
        // box's caret. `inflight_tool.is_none()` flips back as soon as
        // the call commits, so the cursor reappears at the input box on
        // the very next 5ms paint tick.
        let suppress_cursor = self.live_spinner_active || self.inflight_tool.is_some();
        self.screen.set_cursor_visible(!suppress_cursor);
    }

    /// Footer total height — mirrors the computation inside
    /// `paint_footer` so `paint_body` knows where body_bottom lands.
    fn current_footer_rows(&self) -> usize {
        // Mirror paint_footer: input box is full-width (only "> " prefix).
        let text_budget = (self.screen.width() as usize).saturating_sub(2);
        let safe = scrub_controls(&self.input_buf);
        let middle_rows = if text_budget == 0 {
            1
        } else {
            crate::width::wrap_with_cursor(&safe, text_budget, self.input_cursor_byte)
                .0
                .len()
                .max(1)
        };
        let menu_rows = self
            .menu
            .as_ref()
            .map(|m| m.items.len().min(4))
            .unwrap_or(0);
        let has_status = !self.status.model.is_empty()
            || !self.status.cwd.is_empty()
            || self.status.hint.is_some();
        let status_rows = if has_status { 1 } else { 0 };
        let attachment_rows = self.input_attachments.len();
        // 1 top rule + middle + 1 bot rule + attachments + menu + status.
        // (Spinner used to reserve a row here but now lives in body as
        // a live paragraph — see `push_or_update_live_spinner`.)
        1 + middle_rows + 1 + attachment_rows + menu_rows + status_rows
    }

    /// Single-entry-point for painting a full frame. Body is already
    /// on-screen (written append-style by `emit_body_line`), so the
    /// frame paint just refreshes the footer strip + DECSTBM region.
    fn paint_frame(&mut self) {
        self.ensure_scroll_region();
        self.paint_footer();
    }

    /// 1-indexed row of the bottom line of the body area (= top of
    /// the footer strip minus 1). `0` means "footer occupies the
    /// whole viewport" — in that pathological case we skip body
    /// emit entirely rather than clobber the footer.
    fn body_bottom_row(&self) -> u16 {
        let h = self.screen.height() as usize;
        let footer_rows = self.current_footer_rows();
        h.saturating_sub(footer_rows) as u16
    }

    /// Sync the terminal's DECSTBM scroll region with the current
    /// body_bottom. Called at the top of `paint_frame` and before
    /// every body-line emit so `\n` in `emit_body_line` only scrolls
    /// the body strip — the footer stays pinned below.
    ///
    /// When the footer grows (body shrinks), we just shrink the
    /// region: the footer's own cell-diff paint will overwrite any
    /// body text that now lives in footer rows. When the footer
    /// shrinks (body grows), rows that were formerly footer need
    /// a physical wipe — easier to just clear+reflow the body
    /// tail than to track which rows dirty; viewport-only clear
    /// preserves scrollback.
    fn ensure_scroll_region(&mut self) {
        let bottom = self.body_bottom_row();
        if bottom == 0 {
            // Footer fills the viewport; release any region so
            // subsequent paints behave like classic full-screen.
            if self.scroll_region_bottom.is_some() {
                let _ = self.out.write_all(b"\x1b[r");
                self.scroll_region_bottom = None;
            }
            return;
        }
        if self.scroll_region_bottom == Some(bottom) {
            return;
        }
        // Capture the old region bottom BEFORE swapping in the new
        // value — needed by the repaint branch below to know which
        // rows may still hold stale body glyphs.
        let prev_bottom = self.scroll_region_bottom;
        let changed = matches!(prev_bottom, Some(prev) if prev != bottom);
        // Set the new region. 1-indexed, inclusive: `\x1b[1;N r`.
        // Pre-format into one buffer so the write hits the stream as
        // a single call — BufWriter's `write!` can fragment into 3-4
        // tiny write calls otherwise (Display adapter path), which
        // the chunk-counting test harness then observes as separate
        // "chunks" below the 512 B threshold.
        let seq = format!("\x1b[1;{}r", bottom);
        let _ = self.out.write_all(seq.as_bytes());
        self.scroll_region_bottom = Some(bottom);
        if changed {
            // Region shifted (footer grew or shrank). The visible
            // body rows are now misaligned with body_lines — either
            // stale body glyphs sit in what are now footer rows, or
            // new blank rows opened up above the footer. Repaint
            // the body in place so the viewport matches body_lines.
            //
            // CRITICAL — two constraints that together rule out the
            // obvious "2J + re-emit" approach:
            //
            //  1. No `\n`-based re-emit. `emit_body_line_inner` writes
            //     LF at region bottom, which promotes the region-top
            //     row into scrollback on every call. Each cached body
            //     row already scrolled into scrollback once during its
            //     original emit; re-emitting via LF here duplicates
            //     those rows in scrollback (user report: "往上翻会看
            //     到重复内容残留" after `/model`).
            //
            //  2. No `\x1b[2J`. macOS Terminal.app, iTerm2, and xterm
            //     with `cbScrollback` copy every non-blank visible row
            //     into scrollback when processing ED. That means the
            //     very first footer-height transition after startup
            //     (status line appears, body_bottom shrinks by 1)
            //     shoves the whole welcome banner into scrollback
            //     before we get a chance to repaint it (user report:
            //     "首次启动都出现了两次，上面的不带输入框").
            //
            // Instead: paint the tail of body_lines at absolute
            // positions with per-row EL (`\x1b[K`) for any stale
            // content, invalidate the cell cache so the footer diff
            // repaints rows fresh below body_bottom, and explicitly
            // erase the narrow "transition zone" — rows that changed
            // zone between old and new layouts and can't rely on
            // either writer to clean them:
            //
            //  * SHRINK: rows (new_bottom+1)..=prev_bottom were body,
            //    now footer. Footer diff would paint blank cells for
            //    those rows (e.g., the spinner slot when no spinner
            //    is active), but invalidated prev_cells are also
            //    blank → diff skips blank→blank and stale body
            //    glyphs persist. Symptom of the first-startup bug:
            //    welcome's last row "leaks" into the spinner slot.
            //
            //  * GROW: rows (prev_body_top)..(new_body_top) were the
            //    top of the old body but now sit above the new body
            //    anchor and aren't covered by either painter
            //    ("zombie zone" — fixed the `/` then Esc ghost
            //    regression).
            //
            // Per-row EL is row-local (no scroll, no ED) so it can't
            // leak content into scrollback the way `\x1b[2J` does on
            // macOS Terminal.app / iTerm2.
            let cap = bottom as usize;
            let total = self.body_lines.len();
            let start = total.saturating_sub(cap);
            let visible_count = total - start;

            if let Some(prev) = prev_bottom.map(|v| v as usize) {
                // Erase the union of old and new footer regions
                // (rows min(prev,cap)+1 ..= h).
                //
                // Why the full union: the footer writer after this
                // runs `invalidate()` (prev_cells all blank) and
                // then only emits patches where new cells differ
                // from blank. `pad_row_to_width` fills middle /
                // spinner / absent-menu rows with default-style
                // blanks — those match prev blanks → no erase
                // patches. Meanwhile the terminal still holds the
                // prior frame's top_rule / bot_rule `─`-filled
                // cells at rows that are now blank in the new
                // layout.
                //
                // Two symptoms this protects against:
                //   * SHRINK: `❯ 1─────` — new middle content sits
                //     at an absolute row that used to be top_rule;
                //     the rule tail bleeds through.
                //   * GROW: Shift+Enter then delete leaves an
                //     extra ─── line above the input box — the
                //     old top_rule row lands on the new spinner
                //     slot (paint_footer writes a blank row there
                //     when no spinner is active), cell diff sees
                //     blank→blank, stale rule persists.
                //
                // Cost: a small handful of CUP+EL pairs per footer
                // resize (not per frame). EL is row-local → no
                // scroll, no scrollback pollution.
                let screen_h = self.screen.height() as usize;
                let transition_start = prev.min(cap) + 1;
                for row in transition_start..=screen_h {
                    let seq = format!("\x1b[{};1H\x1b[K", row);
                    let _ = self.out.write_all(seq.as_bytes());
                }

                // Grow case only: the "zombie zone" above the new
                // body anchor — rows that held the top of the old
                // body but sit above the new body position and
                // aren't covered by either body paint or footer
                // diff. Fixed the menu-close ghost welcome
                // regression.
                if prev < cap && visible_count > 0 {
                    let prev_body_top = prev.saturating_sub(visible_count) + 1;
                    let new_body_top = cap.saturating_sub(visible_count) + 1;
                    if prev_body_top < new_body_top {
                        for row in prev_body_top..new_body_top {
                            let seq = format!("\x1b[{};1H\x1b[K", row);
                            let _ = self.out.write_all(seq.as_bytes());
                        }
                    }
                }
            }

            self.screen.invalidate();

            let start_row = (cap - visible_count) as u16 + 1;
            // Clone once; serialize_row borrows immutably, the
            // write borrows &mut self.out which is disjoint from
            // body_lines.
            let rows: Vec<Vec<Cell>> = self.body_lines[start..].to_vec();
            for (i, row) in rows.iter().enumerate() {
                let seq = format!("\x1b[{};1H\x1b[K", start_row + i as u16);
                let _ = self.out.write_all(seq.as_bytes());
                let bytes = serialize_row(row);
                let _ = self.out.write_all(&bytes);
            }
            // Park the cursor at the bottom of the body region so
            // the next `emit_body_line_inner` (with `\n` at bottom)
            // behaves the same as if the region had been stable all
            // along.
            let seq = format!("\x1b[{};1H", bottom);
            let _ = self.out.write_all(seq.as_bytes());
        }
    }

    /// Write one body row to stdout at the bottom of the scroll
    /// region, scrolling the region up one line (oldest line enters
    /// scrollback, DECSTBM contains the scroll to the body strip).
    /// Assumes `ensure_scroll_region` has already set the region.
    ///
    /// When `skip_body_scroll_count` is non-zero (see `pop_approval_prompt`),
    /// the LF is skipped — the new row overwrites whatever was sitting
    /// at body_bottom (typically the freshly-popped approval prompt)
    /// so the visual flow `▸ Tool` → `⎿ result` has no gap.
    fn emit_body_line_inner(&mut self, row: &[Cell], bottom: u16) {
        if self.view_mode {
            // In view_mode the body_lines buffer is the source of truth and
            // paint_body repaints from buffer. Don't write to terminal here —
            // we'd overwrite scrolled-away content.
            return;
        }
        // `\x1b[K` (EL — erase from cursor to end of line) runs AFTER
        // reposition and BEFORE writing the row. ECMA-48 says SU at
        // bottom of a scroll region must blank the new bottom row, but
        // Terminal.app and iTerm2 both leave stale cells there when the
        // source content was wider than the new row. Without the
        // explicit erase, short rows (e.g., "> hi", "(cancelled)", an
        // empty spacer) let the previous row's tail bleed through —
        // classic symptom was `/provider  to add a custom model` from
        // the welcome banner leaking past shorter subsequent rows.
        if self.skip_body_scroll_count > 0 {
            // In-place overwrite: position + erase, no LF (so the
            // body region isn't shifted up; the prior approval prompt
            // at body_bottom gets replaced cleanly). Each skipped
            // scroll closes one row of the gap left by
            // pop_approval_prompt.
            let target = bottom.saturating_sub(self.skip_body_scroll_count - 1);
            let seq = format!("\x1b[{};1H\x1b[K", target);
            let _ = self.out.write_all(seq.as_bytes());
            self.skip_body_scroll_count -= 1;
        } else {
            let seq = format!("\x1b[{};1H\n\x1b[{};1H\x1b[K", bottom, bottom);
            let _ = self.out.write_all(seq.as_bytes());
        }
        let bytes = serialize_row(row);
        let _ = self.out.write_all(&bytes);
    }

    /// Erase the live spinner if one is active: pop the transient
    /// last row from `body_lines`, wipe its cells from the terminal
    /// at `body_bottom`, and clear the active flag. Returns true iff
    /// a clear actually happened, so callers (e.g. `push_body_row`)
    /// can arrange for their replacement row to overwrite in-place
    /// instead of scrolling.
    ///
    /// The spinner is treated as an in-progress indicator, not a
    /// historical paragraph header: any transition away from it
    /// (assistant text arriving, tool call pushing, user returning
    /// to the input prompt) means the row's purpose is done and it
    /// should disappear without residue — that matches what users
    /// expected from the old footer-based spinner (cell diff
    /// naturally cleared it on the next frame).
    fn clear_live_spinner(&mut self) -> bool {
        if !self.live_spinner_active {
            return false;
        }
        self.live_spinner_active = false;
        // The cursor will be re-shown on the next paint_footer (which
        // sees live_spinner_active=false and calls set_cursor_visible(true)).
        self.body_lines.pop();
        self.ensure_scroll_region();
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            let seq = format!("\x1b[{};1H\x1b[K", bottom);
            let _ = self.out.write_all(seq.as_bytes());
        }
        true
    }

    /// Append a fully-cell-formatted body row to history AND emit it
    /// immediately so it enters terminal scrollback. Trims oldest
    /// `body_lines` when over the retention cap (memory-only — rows
    /// already pushed to scrollback live on in the terminal's buffer).
    ///
    /// If a live spinner row is currently sitting at `body_bottom`,
    /// erase it first and overwrite in-place: the spinner is
    /// transient, the new row takes its slot without scrolling other
    /// history up by one.
    fn push_body_row(&mut self, row: Vec<Cell>) {
        // Any external body push freezes an active live-group: the
        // group's child rows are no longer guaranteed to sit at the
        // bottom (they may have scrolled into native scrollback the
        // moment this push commits a `\n`). Future ToolGroupChildUpdate
        // events fall back to no-op rather than CUP-rewriting some
        // unrelated row that took the group child's screen position.
        self.live_group = None;
        if self.clear_live_spinner() {
            // In-place overwrite at `body_bottom` — `emit_body_line_inner`
            // honours this flag to skip its LF and just CUP+EL+write at
            // the current bottom row. That way the slot previously held
            // by the spinner becomes the slot for this new body row,
            // with no intervening blank line.
            self.skip_body_scroll_count = self.skip_body_scroll_count.saturating_add(1);
        }
        // Region might be stale (first call after resume, or footer
        // just changed); sync before emit so the LF in emit_body_line
        // scrolls only within the body strip.
        self.ensure_scroll_region();
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            self.emit_body_line_inner(&row, bottom);
        }
        self.body_lines.push(row);
        if self.body_lines.len() > MAX_SCROLLBACK_ROWS {
            let drain = self.body_lines.len() - MAX_SCROLLBACK_ROWS;
            self.body_lines.drain(0..drain);
            self.message_marks.retain(|m| m.line_idx >= drain);
            for m in self.message_marks.iter_mut() {
                m.line_idx -= drain;
            }
        }
    }

    /// Record the start of a new logical message in `message_marks`.
    /// The mark's `line_idx` is set to the CURRENT length of `body_lines`
    /// (i.e. the index the NEXT `push_body_row` will occupy).
    /// Called before any push in the render arm that starts a new message.
    fn mark_message(&mut self, kind: crate::render::MarkKind) {
        self.message_marks.push(crate::render::MessageMark {
            line_idx: self.body_lines.len(),
            kind,
        });
    }

    /// Push or update the live spinner body row. On the first call of a
    /// run it pushes fresh via `push_body_row` and marks the row live.
    /// On subsequent calls (every tick), it REPLACES `body_lines.last()`
    /// and re-emits at absolute `body_bottom_row()` without the
    /// `\n`-scroll — that way 80ms animation frames don't each push a
    /// new row into scrollback and don't scroll the user's real history
    /// off-screen.
    fn push_or_update_live_spinner(&mut self, row_cells: Vec<Cell>) {
        if self.live_spinner_active {
            if let Some(last) = self.body_lines.last_mut() {
                *last = row_cells.clone();
            }
            self.ensure_scroll_region();
            let bottom = self.body_bottom_row();
            if bottom > 0 {
                let seq = format!("\x1b[{};1H\x1b[K", bottom);
                let _ = self.out.write_all(seq.as_bytes());
                let bytes = serialize_row(&row_cells);
                let _ = self.out.write_all(&bytes);
            }
        } else {
            // `push_body_row` clears `live_spinner_active`; set it back
            // afterwards so the next tick takes the update-in-place
            // branch above.
            self.push_body_row(row_cells);
            self.live_spinner_active = true;
        }
        // Cursor visibility is driven by `paint_footer` reading
        // `live_spinner_active` — see set_cursor_visible call there.
        // No direct DECTCEM write here, otherwise the next render_diff
        // would re-emit \x1b[?25h based on screen.cursor_visible and
        // visually undo our hide on a 5ms cadence.
    }

    /// Freeze the current inflight_tool row into the body transcript
    /// using `push_body_prefixed` so long commands are properly wrapped
    /// across multiple terminal lines. Used as the uniform commit path
    /// for: `ToolCallCommit`, `TurnComplete`, `TurnCancelled`, and the
    /// `ToolResult` fallback — same wrapping pipeline as
    /// `render_inflight_tool` but pushes a frozen `▸` icon and clears
    /// `inflight_tool_rows` so the next live tick starts fresh.
    fn commit_inflight_tool(&mut self) {
        if let Some((_id, name, detail)) = self.inflight_tool.take() {
            let safe_name = scrub_controls(&name);
            let safe_detail = scrub_controls(&detail);
            let body_str = if safe_detail.is_empty() {
                safe_name
            } else {
                format!("{}({})", safe_name, safe_detail)
            };
            // Safety cap: prevent degenerate bodies (e.g. multi-KB bash
            // commands) from producing hundreds of terminal lines.
            let body_str = truncate_body_str(&body_str, 500);
            // Clear any previously rendered inflight tool rows so
            // push_body_prefixed appends fresh committed lines.
            self.live_spinner_active = false;
            let remove = self.inflight_tool_rows.min(self.body_lines.len());
            self.body_lines.truncate(self.body_lines.len() - remove);
            self.inflight_tool_rows = 0;
            self.ensure_scroll_region();
            let bottom = self.body_bottom_row();
            if bottom > 0 && remove > 0 {
                // Erase ALL terminal rows previously occupied by the
                // inflight spinner (may be >1 when the command was long
                // enough to wrap). Without this, the old `⠙ Bash(...)`
                // row lingers on-screen above the freshly committed
                // `● Bash(...)` row, producing a visual duplicate.
                let start_row = bottom.saturating_sub(remove as u16 - 1).max(1);
                let mut seq = String::with_capacity((bottom - start_row + 1) as usize * 8);
                use std::fmt::Write as _;
                for row in start_row..=bottom {
                    let _ = write!(seq, "\x1b[{};1H\x1b[K", row);
                }
                let _ = self.out.write_all(seq.as_bytes());
            }
            // The CUP+EL above erased the inflight rows in place — the
            // committed rows should land in those exact slots. Without
            // this flag, `push_body_prefixed`'s underlying
            // `emit_body_line_inner` emits an LF that scrolls the body
            // region up by one, leaving the just-erased row as a
            // second blank between the user message and the committed
            // tool call (visible as the `> question \n \n ● tool`
            // double-gap in screenshots). Use `remove` (not just 1)
            // so multi-row inflight spinners are fully covered.
            self.skip_body_scroll_count = self.skip_body_scroll_count.saturating_add(remove as u16);
            self.push_body_prefixed(
                // Frozen icon matches the static ToolCall arm — see its
                // comment for the Windows-font rationale that picked ●
                // (U+25CF, Geometric Shapes block) over ▸ (U+25B8,
                // missing from Consolas/NSimSun and rendered as `□`
                // tofu in screenshots).
                "\u{25cf} ",
                &self.style_for(Role::Muted),
                &body_str,
                &self.style_bold(Role::ToolName),
            );
        }
    }

    /// Copy the visible body tail into the host terminal's native
    /// scrollback before we wipe the viewport on exit. Retained mode
    /// keeps the newest body rows pinned on screen behind a fixed
    /// footer; those rows have not naturally scrolled off yet, so a
    /// plain viewport clear would make the bottom of the transcript
    /// disappear after `/quit`.
    fn promote_visible_body_to_scrollback(&mut self) {
        let bottom = self.body_bottom_row() as usize;
        if bottom == 0 || self.body_lines.is_empty() {
            return;
        }

        let screen_w = self.screen.width() as usize;
        let screen_h = self.screen.height() as usize;
        let n = self.body_lines.len().min(bottom);
        if n == 1 && screen_h < 2 {
            return;
        }
        let start = self.body_lines.len() - n;
        let rows: Vec<Vec<Cell>> = self.body_lines[start..]
            .iter()
            .map(|row| clip_cells_to_width(row, screen_w))
            .collect();

        // Repaint the visible transcript tail at the top of a temporary
        // top-anchored scroll region, then LF each row out of that
        // region. Top-anchored DECSTBM is the path terminals promote
        // into native scrollback; absolute repainting itself has no
        // scrollback side effect.
        let region_bottom = if n == 1 { 2 } else { n } as u16;
        let seq = format!("\x1b[1;{}r", region_bottom);
        let _ = self.out.write_all(seq.as_bytes());
        for (i, row) in rows.iter().enumerate() {
            let seq = format!("\x1b[{};1H\x1b[K", i + 1);
            let _ = self.out.write_all(seq.as_bytes());
            let bytes = serialize_row(row);
            let _ = self.out.write_all(&bytes);
        }
        if region_bottom as usize > n {
            let seq = format!("\x1b[{};1H\x1b[K", region_bottom);
            let _ = self.out.write_all(seq.as_bytes());
        }
        let seq = format!("\x1b[{};1H", region_bottom);
        let _ = self.out.write_all(seq.as_bytes());
        for _ in 0..n {
            let _ = self.out.write_all(b"\n");
        }
        self.scroll_region_bottom = Some(region_bottom);
    }

    /// Wrap `text` to content width and push each wrapped chunk as
    /// its own body row with a PAD_COL prefix. Used by variants
    /// whose content is plain (assistant text, command output).
    fn push_body_text(&mut self, text: &str, style: &CellStyle) {
        let w = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        if w == 0 {
            return;
        }
        // `text.split('\n')` on `"foo\n"` yields `["foo", ""]` and the
        // empty chunk pushes a blank row. Callers rely on this to add
        // a trailing breathing-row after their content (e.g. the
        // bash `Ctrl+O` hint, status echoes from `/model`/`/login`).
        // Internal `\n`s split into multiple rows. Don't pre-strip the
        // trailing `\n` — that's a meaningful "give me a separator"
        // signal at the call site, not noise.
        for phys in text.split('\n') {
            for chunk in crate::width::wrap_line_to_width(phys, w) {
                let mut row = Vec::new();
                let pad = CellStyle::default();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
                push_str_cells(&mut row, &chunk, style);
                self.push_body_row(row);
            }
        }
    }

    /// SGR-aware variant of `push_body_text` for **trusted** content
    /// that may carry inline `\x1b[...m` colour / bold / faint /
    /// reverse spans (e.g. the `/codingplan` setup report's red
    /// locked-model rows). Splits on `\n`, wraps each physical line,
    /// and feeds each chunk through `push_str_cells_sgr` so the
    /// working style mutates as cells are produced. SGR state resets
    /// at every `\n` so a forgotten reset doesn't bleed colour into
    /// the next logical row.
    ///
    /// Only used from the `UiLine::CommandOutput` arm — every other
    /// caller has plain text and stays on the simpler
    /// `push_body_text`.
    fn push_body_text_sgr(&mut self, text: &str) {
        let w = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        if w == 0 {
            return;
        }
        for phys in text.split('\n') {
            let mut style = CellStyle::default();
            for chunk in crate::width::wrap_line_to_width(phys, w) {
                let mut row = Vec::new();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &CellStyle::default());
                style = crate::render::cell::push_str_cells_sgr(&mut row, &chunk, style);
                self.push_body_row(row);
            }
        }
    }

    /// Build one row with a leading `prefix` (often an accent
    /// glyph with its own style) and a plain-styled body. Used by
    /// User echo ("> …"), ToolCall ("▸ name(detail)"), etc.
    ///
    /// Multi-line `body` (Shift+Enter in the input, or a tool detail
    /// that happens to contain `\n`) is split on '\n' BEFORE width
    /// wrapping — otherwise the newlines ride through as width-1 cells
    /// and `serialize_row` writes them to stdout as bare LF bytes,
    /// which under raw-mode + DECSTBM produces the staircase pattern
    /// (cursor drops a row without returning to col 1, every LF also
    /// triggers a region scroll).
    fn push_body_prefixed(
        &mut self,
        prefix: &str,
        prefix_style: &CellStyle,
        body: &str,
        body_style: &CellStyle,
    ) {
        let rows = self.build_prefixed_rows(prefix, prefix_style, body, body_style);
        for row in rows {
            self.push_body_row(row);
        }
    }

    /// Symbol-anchored row builder. Wraps `body` to `screen_width − PAD_COL`,
    /// emits the leading row with `prefix`, continuation rows with a blank
    /// pad of equal display width. Pure: no side effects on `body_lines`
    /// or terminal output. Used by `push_body_prefixed` (which appends each
    /// row via push_body_row) and `render_inflight_tool` (which writes
    /// in-place over previously-rendered inflight rows during spinner
    /// ticks — see that fn's doc comment for the scrollback-leak bug
    /// this split addresses).
    fn build_prefixed_rows(
        &self,
        prefix: &str,
        prefix_style: &CellStyle,
        body: &str,
        body_style: &CellStyle,
    ) -> Vec<Vec<Cell>> {
        let w = (self.screen.width() as usize).saturating_sub(PAD_COL);
        if w == 0 {
            return Vec::new();
        }
        let prefix_w = crate::width::display_width(prefix);
        let first_budget = w.saturating_sub(prefix_w);
        let cont_pad: String = " ".repeat(prefix_w);
        let mut rows = Vec::new();
        let mut first_emitted = false;
        for phys in body.split('\n') {
            let chunks: Vec<String> = crate::width::wrap_line_to_width(phys, first_budget.max(1))
                .into_iter()
                .map(|c| c.to_string())
                .collect();
            for chunk in &chunks {
                let mut row = Vec::new();
                let pad = CellStyle::default();
                if !first_emitted {
                    push_str_cells(&mut row, prefix, prefix_style);
                    first_emitted = true;
                } else {
                    push_str_cells(&mut row, &cont_pad, &pad);
                }
                push_str_cells(&mut row, chunk.as_str(), body_style);
                rows.push(row);
            }
        }
        rows
    }

    /// Flush complete lines (those terminated by `\n`) from the
    /// streaming assistant buffer into `body_lines`, rendering
    /// each through the markdown inline renderer so bold / inline
    /// code / lists / headings get their styled cells.
    fn flush_assistant_lines(&mut self) {
        if !self.assistant_line_buf.contains('\n') {
            return;
        }
        let md_width = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        let mut completed: Vec<String> = Vec::new();
        while let Some(nl) = self.assistant_line_buf.find('\n') {
            let line: String = self.assistant_line_buf.drain(..=nl).collect();
            let content = line[..line.len() - 1].to_string();
            if let Some(rendered) = crate::markdown::render_line_with_width(
                &content,
                &mut self.md_state,
                self.caps,
                md_width,
            ) {
                completed.push(rendered);
            }
        }
        for rendered in completed {
            self.push_markdown_body(&rendered);
        }
    }

    /// Turn the partial buffer into a body row (as if `\n`
    /// terminated). Called on AssistantLineBreak / TurnComplete.
    /// Also drains any trailing markdown block buffer (tables that
    /// ended without a following non-table line).
    fn flush_assistant_remainder(&mut self) {
        let md_width = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        if !self.assistant_line_buf.is_empty() {
            let line = std::mem::take(&mut self.assistant_line_buf);
            if let Some(rendered) = crate::markdown::render_line_with_width(
                &line,
                &mut self.md_state,
                self.caps,
                md_width,
            ) {
                self.push_markdown_body(&rendered);
            }
        }
        if let Some(block) =
            crate::markdown::finalize_with_width(&mut self.md_state, self.caps, md_width)
        {
            self.push_markdown_body(&block);
        }
    }

    /// Parse a markdown-rendered string (ANSI-tinted) into cells
    /// and push each wrapped line to body history. Wrap is done
    /// at cell level (not byte level) so wide glyphs and SGR
    /// state survive the split.
    fn push_markdown_body(&mut self, rendered: &str) {
        let w = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        if w == 0 {
            return;
        }
        // Collapse consecutive blank assistant lines. Some models
        // (MiniMax-M2.7 in particular) emit `\n\n\n…` between tool
        // calls and paragraphs; verbatim rendering produces multi-row
        // vertical gaps that feel "unfinished". Allow at most one
        // blank row in a row — enough for paragraph separation,
        // nothing more.
        //
        // Special case: when the live spinner is the tail row, also
        // skip blank pushes. Many models emit a leading `\n` warm-up
        // before the first real reply chunk. Without this, that
        // leading blank evicts the spinner + leaves a ghost blank
        // row that the NEXT (non-blank) chunk then scrolls above
        // the real content — producing a visible double-blank
        // between the user message and the assistant reply. The
        // spinner itself is transient (not a historical paragraph),
        // so there's no paragraph boundary here worth marking with
        // a blank.
        let is_blank = rendered.trim().is_empty();
        if is_blank {
            let tail_blank = self
                .body_lines
                .last()
                .map(|r| r.iter().all(|c| c.ch == ' '))
                .unwrap_or(true);
            if tail_blank || self.live_spinner_active {
                return;
            }
        }
        let lines_of_cells = parse_markdown_to_cells(rendered);
        for line_cells in lines_of_cells {
            let chunks = wrap_cells_to_width(&line_cells, w);
            for chunk in chunks {
                let mut row = Vec::new();
                let pad = CellStyle::default();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
                row.extend(chunk);
                self.push_body_row(row);
            }
        }
    }

    fn flush_frame(&mut self) {
        let bytes = self.screen.render_diff();
        let _ = self.out.write_all(&bytes);
    }

    fn build_prefixed_wrapped_rows(
        &self,
        prefix: &str,
        prefix_style: &CellStyle,
        continuation_prefix: &str,
        continuation_style: &CellStyle,
        content: Vec<Cell>,
        content_width: usize,
    ) -> Vec<Vec<Cell>> {
        let prefix_w = crate::width::display_width(prefix);
        let cont_prefix_w = crate::width::display_width(continuation_prefix);
        let first_budget = content_width.saturating_sub(prefix_w).max(1);
        let cont_budget = content_width.saturating_sub(cont_prefix_w).max(1);

        let first_chunks = wrap_cells_to_width(&content, first_budget);
        let mut rows = Vec::with_capacity(first_chunks.len().max(1));
        for (idx, chunk) in first_chunks.into_iter().enumerate() {
            let mut row = Vec::new();
            let pad = CellStyle::default();
            push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
            if idx == 0 {
                push_str_cells(&mut row, prefix, prefix_style);
            } else {
                push_str_cells(&mut row, continuation_prefix, continuation_style);
            }
            row.extend(chunk);
            rows.push(row);
        }
        if rows.len() <= 1 {
            return rows;
        }

        let mut normalized = Vec::new();
        let mut first = true;
        for row in rows {
            if first {
                normalized.push(row);
                first = false;
                continue;
            }

            let mut content_only = row;
            let strip = PAD_COL + cont_prefix_w;
            content_only.drain(..strip.min(content_only.len()));

            let mut wrapped = wrap_cells_to_width(&content_only, cont_budget);
            for chunk in wrapped.drain(..) {
                let mut next = Vec::new();
                let pad = CellStyle::default();
                push_str_cells(&mut next, &" ".repeat(PAD_COL), &pad);
                push_str_cells(&mut next, continuation_prefix, continuation_style);
                next.extend(chunk);
                normalized.push(next);
            }
        }
        normalized
    }

    fn build_wrapped_text_rows(
        &self,
        parts: &[(&str, CellStyle)],
        content_width: usize,
    ) -> Vec<Vec<Cell>> {
        let mut content = Vec::new();
        for (text, style) in parts {
            push_str_cells(&mut content, text, style);
        }
        let chunks = wrap_cells_to_width(&content, content_width.max(1));
        let mut rows = Vec::with_capacity(chunks.len().max(1));
        for chunk in chunks {
            let mut row = Vec::new();
            let pad = CellStyle::default();
            push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
            row.extend(chunk);
            rows.push(row);
        }
        rows
    }

    fn build_welcome_rows(&self, model: &str, working_dir: &str) -> Vec<Vec<Cell>> {
        // Mirror AnsiRenderer::render_welcome, but allow narrow terminals
        // to reflow path/model/tips instead of truncating or colliding.
        let w = self.screen.width() as usize;
        let content_w = w.saturating_sub(PAD_COL * 2).max(1);
        // Row 1: brand left + version · license right
        let left_txt = "◆ AtomCode";
        let right_ver = concat!("v", env!("CARGO_PKG_VERSION"));
        let right_lic = "MIT";
        let left_w = crate::width::display_width(left_txt);
        let right_txt = format!("{}  ·  {}", right_ver, right_lic);
        let right_w = crate::width::display_width(&right_txt);
        let mut rows = Vec::with_capacity(6);
        let pad = CellStyle::default();
        if content_w > left_w + right_w {
            let gap = content_w.saturating_sub(left_w + right_w);
            let mut row1 = Vec::new();
            push_str_cells(&mut row1, &" ".repeat(PAD_COL), &pad);
            push_str_cells(&mut row1, left_txt, &self.style_bold(Role::Brand));
            for _ in 0..gap {
                row1.push(Cell::blank());
            }
            push_str_cells(&mut row1, right_ver, &self.style_for(Role::Secondary));
            push_str_cells(&mut row1, "  ·  ", &self.style_for(Role::Muted));
            push_str_cells(&mut row1, right_lic, &self.style_for(Role::Muted));
            rows.push(row1);
        } else {
            let mut row1 = Vec::new();
            push_str_cells(&mut row1, &" ".repeat(PAD_COL), &pad);
            push_str_cells(&mut row1, left_txt, &self.style_bold(Role::Brand));
            rows.push(row1);

            let right_gap = content_w.saturating_sub(right_w);
            let mut row1b = Vec::new();
            push_str_cells(&mut row1b, &" ".repeat(PAD_COL), &pad);
            for _ in 0..right_gap {
                row1b.push(Cell::blank());
            }
            push_str_cells(&mut row1b, right_ver, &self.style_for(Role::Secondary));
            push_str_cells(&mut row1b, "  ·  ", &self.style_for(Role::Muted));
            push_str_cells(&mut row1b, right_lic, &self.style_for(Role::Muted));
            rows.push(row1b);
        }

        let bullet_style = self.style_for(Role::AccentDim);
        let secondary_style = self.style_for(Role::Secondary);
        let path_cells = {
            let mut cells = Vec::new();
            push_str_cells(&mut cells, working_dir, &secondary_style);
            cells
        };
        rows.extend(self.build_prefixed_wrapped_rows(
            "∙ ",
            &bullet_style,
            "  ",
            &CellStyle::default(),
            path_cells,
            content_w,
        ));

        let model_cells = {
            let mut cells = Vec::new();
            push_str_cells(&mut cells, model, &secondary_style);
            cells
        };
        rows.extend(self.build_prefixed_wrapped_rows(
            "∙ ",
            &bullet_style,
            "  ",
            &CellStyle::default(),
            model_cells,
            content_w,
        ));

        // Blank separator.
        rows.push(Vec::new());

        // Hint rows. The prose around the slash shortcuts is onboarding-
        // critical text — first thing a new user reads. Use faint
        // (SGR 2) over the terminal's default fg so the hint reads as
        // subordinate to primary content without picking a fixed gray
        // (DarkGrey would vanish on some iTerm2 light presets, default
        // fg unmuted competes with the user's input on dark presets).
        // Slash shortcuts stay accent_bold (cyan) for visual emphasis.
        // Hint row(s): input prompt + /provider + /codingplan.
        //
        // Wide enough to fit on one visual row → emit a single combined
        // line (user's preferred shape on standard 100+ col terminals).
        // Narrower → fall back to three separate rows; the alternative
        // is a single line that `build_wrapped_text_rows` would
        // hard-break mid-token (`/provider` → `/provi`+`der`), which
        // looks worse than three short rows on a small terminal.
        let hint_text = self.style_faint(Role::Secondary);
        let accent_bold = self.style_bold(Role::Accent);
        let idle_prefix = t(Msg::IdleHintPrefix);
        let idle_slash = t(Msg::IdleHintSlash);
        let idle_suffix = t(Msg::IdleHintSuffix);
        let provider_cmd = t(Msg::IdleHintProvider);
        let provider_suffix = t(Msg::IdleHintProviderSuffix);
        let codingplan_cmd = t(Msg::IdleHintCodingplan);
        let codingplan_suffix = t(Msg::IdleHintCodingplanSuffix);
        let combined_width: usize = [
            idle_prefix.as_ref(),
            idle_slash.as_ref(),
            idle_suffix.as_ref(),
            "   ",
            provider_cmd.as_ref(),
            "  ",
            provider_suffix.as_ref(),
            "   ",
            codingplan_cmd.as_ref(),
            "  ",
            codingplan_suffix.as_ref(),
        ]
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(*s))
        .sum();
        if combined_width <= content_w {
            rows.extend(self.build_wrapped_text_rows(
                &[
                    (&idle_prefix, hint_text.clone()),
                    (&idle_slash, accent_bold.clone()),
                    (&idle_suffix, hint_text.clone()),
                    ("   ", hint_text.clone()),
                    (&provider_cmd, accent_bold.clone()),
                    ("  ", hint_text.clone()),
                    (&provider_suffix, hint_text.clone()),
                    ("   ", hint_text.clone()),
                    (&codingplan_cmd, accent_bold),
                    ("  ", hint_text.clone()),
                    (&codingplan_suffix, hint_text),
                ],
                content_w,
            ));
        } else {
            rows.extend(self.build_wrapped_text_rows(
                &[
                    (&idle_prefix, hint_text.clone()),
                    (&idle_slash, accent_bold.clone()),
                    (&idle_suffix, hint_text.clone()),
                ],
                content_w,
            ));
            rows.extend(self.build_wrapped_text_rows(
                &[
                    (&provider_cmd, accent_bold.clone()),
                    ("  ", hint_text.clone()),
                    (&provider_suffix, hint_text.clone()),
                ],
                content_w,
            ));
            rows.extend(self.build_wrapped_text_rows(
                &[
                    (&codingplan_cmd, accent_bold),
                    ("  ", hint_text.clone()),
                    (&codingplan_suffix, hint_text),
                ],
                content_w,
            ));
        }

        // Trailing blank so subsequent async events (MCP "已连接",
        // upgrade hints, etc.) don't butt up against the hint row.
        // Mirrors alt_screen's push_welcome trailing blank.
        rows.push(Vec::new());

        rows
    }

    fn push_welcome(&mut self, model: &str, working_dir: &str) {
        let rows = self.build_welcome_rows(model, working_dir);
        self.welcome_banner = Some((model.to_string(), working_dir.to_string()));
        self.welcome_line_count = rows.len();
        for row in rows {
            self.push_body_row(row);
        }
    }

    fn reflow_welcome_prefix(&mut self) {
        let Some((ref model, ref working_dir)) = self.welcome_banner else {
            return;
        };
        if self.welcome_line_count == 0 || self.body_lines.len() < self.welcome_line_count {
            return;
        }
        let rows = self.build_welcome_rows(model, working_dir);
        let new_len = rows.len();
        self.body_lines
            .splice(0..self.welcome_line_count, rows.into_iter());
        self.welcome_line_count = new_len;
    }

    /// Force a fresh paint of body region rows from body_lines.
    /// In view_mode: paint body_lines[viewport_top..viewport_top+body_height].
    /// Out of view_mode (just exited): paint body_lines tail.
    /// Always uses CUP+EL+content per row; never emits LF.
    fn repaint_body_region(&mut self) {
        let bottom = self.body_bottom_row();
        if bottom == 0 || self.body_lines.is_empty() {
            return;
        }
        let body_height = bottom as usize;
        let total = self.body_lines.len();
        let start = if self.view_mode {
            self.viewport_top.min(total.saturating_sub(1))
        } else {
            total.saturating_sub(body_height)
        };
        let end = (start + body_height).min(total);
        // Clone the slice to avoid simultaneous borrow of self.
        let rows: Vec<Vec<Cell>> = self.body_lines[start..end].to_vec();
        for (i, row) in rows.iter().enumerate() {
            let target_row = 1 + i as u16;
            let seq = format!("\x1b[{};1H\x1b[K", target_row);
            let _ = self.out.write_all(seq.as_bytes());
            let bytes = serialize_row(row);
            let _ = self.out.write_all(&bytes);
        }
        // Clear any rows below content (when body_lines is short).
        for i in (end - start)..body_height {
            let target_row = 1 + i as u16;
            let seq = format!("\x1b[{};1H\x1b[K", target_row);
            let _ = self.out.write_all(seq.as_bytes());
        }
        let _ = self.out.flush();
        self.screen.invalidate();
    }
}

impl<W: Write + Send> Renderer for RetainedRenderer<W> {
    fn render(&mut self, line: UiLine) {
        match line {
            // ── footer-only variants ──
            UiLine::InputPrompt {
                buf,
                cursor_byte,
                menu,
                status,
                attachments,
            } => {
                // Returning to idle input: the spinner row served its
                // purpose — clear it from both body history and the
                // terminal so the user sees a clean input prompt, not
                // a stale `⠋ Pondering…` row above the input box.
                self.clear_live_spinner();
                self.input_buf = buf;
                self.input_cursor_byte = cursor_byte;
                self.menu = menu;
                self.status = status;
                self.input_attachments = attachments;
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
                // Input box / status / menu still belong in the footer.
                self.input_buf = buf;
                self.input_cursor_byte = cursor_byte;
                self.menu = menu;
                self.status = status;
                self.input_attachments = attachments;
                // Spinner (frame + label) goes into body as a live
                // paragraph header. Each tick replaces the previous
                // wrapped rows via render_inflight_tool so long
                // commands wrap properly (same as committed rows).
                //
                // When a tool call is in flight, the live rows
                // carry the tool-call shape (`<frame> Bash(cmd)`)
                // with the animation driving the icon frame. The
                // spinner label here was built by `format_spinner_label`
                // and carries the ` · 12s · N queued` metadata; pluck
                // that suffix off and forward it to render_inflight_tool
                // so the user gets a time anchor on long bashes.
                if let Some((_id, name, detail)) = self.inflight_tool.clone() {
                    let meta = spinner_meta_suffix(&label);
                    self.render_inflight_tool(frame, &name, &detail, meta);
                } else {
                    let cells = self.build_spinner_body_row(frame, &label);
                    self.push_or_update_live_spinner(cells);
                }
            }
            UiLine::Spinner { frame, label } => {
                if let Some((_id, name, detail)) = self.inflight_tool.clone() {
                    let meta = spinner_meta_suffix(&label);
                    self.render_inflight_tool(frame, &name, &detail, meta);
                } else {
                    let cells = self.build_spinner_body_row(frame, &label);
                    self.push_or_update_live_spinner(cells);
                }
            }
            UiLine::ClearTransient | UiLine::InputCommit => {
                // No-op in retained mode.
                return;
            }

            // ── body: welcome / turn events ──
            UiLine::Welcome { model, working_dir } => {
                let model_scrubbed = scrub_controls(&model);
                let wd_scrubbed = scrub_controls(&working_dir);
                self.push_welcome(&model_scrubbed, &wd_scrubbed);
            }
            UiLine::User(text) => {
                self.mark_message(crate::render::MarkKind::User);
                self.last_mark_was_assistant = false;
                let safe = scrub_controls(&text);
                let accent = self.style_bold(Role::Accent);
                let plain = CellStyle::default();
                self.push_body_prefixed(self.caps.prompt_chevron(), &accent, &safe, &plain);
                // Blank spacer row.
                self.push_body_row(Vec::new());
                // New user turn — reset markdown parser so code-block
                // / table state from previous turn doesn't bleed.
                self.md_state.reset();
            }
            UiLine::TurnSeparator { label } => {
                self.last_mark_was_assistant = false;
                let w = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
                let safe = scrub_controls(&label);
                let lw = crate::width::display_width(&safe);
                let padded = 1 + lw + 1;
                let remaining = w.saturating_sub(padded);
                let left = remaining / 2;
                let right = remaining - left;
                let mut row = Vec::new();
                let pad = CellStyle::default();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &pad);
                // SGR 2 (faint) on the terminal-default fg. This is the
                // quiet "historical" look: the rule and `resumed:` label
                // sit at ~50% intensity so they read as scaffolding, not
                // body text. Previously we used `Role::Muted`, but when
                // MUTED_DARK was widened from SGR 90 → 37 (so tool-batch
                // child rows stay readable on Warp dark), this rule lost
                // its contrast against assistant text. Mirrors the SGR_DIM
                // approach `alt_screen::build_turn_separator` already uses.
                let rule = self.style_faint(Role::Secondary);
                for _ in 0..left {
                    row.push(Cell {
                        ch: '─',
                        style: rule.clone(),
                        width: 1,
                    });
                }
                push_str_cells(&mut row, " ", &pad);
                push_str_cells(&mut row, &safe, &rule);
                push_str_cells(&mut row, " ", &pad);
                for _ in 0..right {
                    row.push(Cell {
                        ch: '─',
                        style: rule.clone(),
                        width: 1,
                    });
                }
                self.push_body_row(Vec::new());
                self.push_body_row(row);
                self.push_body_row(Vec::new());
            }

            // ── body: streaming assistant ──
            UiLine::AssistantText(text) => {
                if !self.last_mark_was_assistant {
                    self.mark_message(crate::render::MarkKind::Assistant);
                    self.last_mark_was_assistant = true;
                }
                self.assistant_line_buf.push_str(&scrub_controls(&text));
                self.flush_assistant_lines();
            }
            UiLine::ReasoningText(text) => {
                // Display reasoning in gray/dimmed style with word wrapping
                let text = scrub_controls(&text);
                // Use ANSI dim/gray escape codes
                let dimmed = format!("\x1b[2m{}\x1b[0m", text);
                self.push_body_text(&dimmed, &CellStyle::default());
            }
            UiLine::AssistantLineBreak => {
                self.flush_assistant_remainder();
            }
            UiLine::TurnComplete => {
                self.flush_assistant_remainder();
                // Defense in depth: a turn that ended without a
                // matching ToolCallCommit (interrupted, forced stop,
                // protocol bug) would otherwise leave inflight_tool
                // set and the next user turn's spinner would mistake
                // the stale tool detail for the in-flight payload.
                // Use push_body_prefixed for proper line wrapping.
                self.commit_inflight_tool();
            }
            UiLine::TurnCancelled => {
                self.flush_assistant_remainder();
                self.commit_inflight_tool();
                // (cancelled) is a state-change marker — must remain
                // visible. Default fg, not Muted.
                let style = self.style_for(Role::Secondary);
                let label = t(Msg::Cancelled);
                self.push_body_text(&label, &style);
            }

            // ── body: tools & diffs ──
            UiLine::ToolCallInFlight { id, name, detail } => {
                self.mark_message(crate::render::MarkKind::ToolCall);
                self.last_mark_was_assistant = false;
                self.flush_assistant_remainder();
                // Parallel tool calls are rare but not impossible. If
                // one is already animating, freeze it before starting
                // a new one — single-at-a-time animation is a deliberate
                // simplification (see field doc).
                if self.inflight_tool.is_some() {
                    // Commit the previous tool (freezes it as ▸ in
                    // the body transcript) before starting a new one.
                    self.commit_inflight_tool();
                }
                // Use a plausible "still" frame for the initial paint;
                // the next Spinner / StreamingBox tick (within ~80ms)
                // overwrites with the real frame, picking up the
                // animation seamlessly.
                let initial = if self.caps.unicode_symbols {
                    "\u{2819}"
                } else {
                    "*"
                };
                self.inflight_tool = Some((id, name.clone(), detail.clone()));
                // Initial paint — no spinner tick has fired yet so no
                // elapsed-time suffix to forward. The next Spinner /
                // StreamingBox tick (~80ms later) supplies the meta.
                self.render_inflight_tool(initial, &name, &detail, "");
            }
            UiLine::ToolCallCommit { call_id } => {
                // Only commit if the inflight_tool matches the expected call_id,
                // or if no call_id was provided (legacy behavior).
                let should_commit = match (call_id, &self.inflight_tool) {
                    (Some(expected_id), Some((actual_id, _, _))) => &expected_id == actual_id,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if should_commit {
                    self.commit_inflight_tool();
                }
            }
            UiLine::ToolGroupRender {
                batch_id,
                header,
                children,
            } => {
                self.flush_assistant_remainder();
                // Push header + N child rows as single-line rows so
                // body_lines indices map 1:1 with terminal positions.
                // push_body_row clears any prior live_group, including
                // ours mid-loop, so we set live_group AFTER the loop.
                //
                // Style:
                // Style:
                // - header: bold, terminal default fg. SGR Color::White
                //   was tried for "亮白" emphasis but on iTerm2's light
                //   preset the terminal maps it to the same shade as
                //   the background — the entire `● Running 3 read_file
                //   calls in parallel` line went invisible (user
                //   screenshot: child rows visible, header line blank).
                //   Same root cause as the inline-code bright-white→
                //   invisible bug fixed in commit 25e9e41 for markdown
                //   code, but unfixed for batch headers until now.
                //   Switching to Role::Secondary (fg=None = `\x1b[39m`
                //   terminal default) means the row picks up whatever
                //   foreground the user's theme set for regular text
                //   — black on light themes, white-ish on dark themes
                //   — and bold supplies the emphasis on both.
                // - children: muted (high-frequency rows, not anchors)
                // - summary: same fix as header (see Summary arm below)
                let header_style = self.style_bold(Role::Secondary);
                let muted = self.style_for(Role::Muted);
                let screen_w = self.screen.width();
                let header_row = build_one_row(&header, &header_style, screen_w);
                self.push_body_row(header_row);
                let header_idx = self.body_lines.len() - 1;

                let mut child_indices: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                for c in &children {
                    let row = build_one_row(&c.text, &muted, screen_w);
                    self.push_body_row(row);
                    child_indices.insert(c.call_id.clone(), self.body_lines.len() - 1);
                }
                self.live_group = Some(LiveGroup {
                    batch_id,
                    header_idx,
                    child_indices,
                });
            }
            UiLine::ToolGroupChildUpdate {
                batch_id,
                call_id,
                new_text,
            } => {
                // CRITICAL: do NOT call flush_assistant_remainder here.
                // It would push pending assistant text via push_body_row,
                // which clears live_group (per the freeze invariant), and
                // the lookup below would silent-return → child never gets
                // its `→ N lines` data. ToolGroupChildUpdate only does a
                // CUP rewrite on an EXISTING body row; it does not create
                // new rows, so there is nothing to flush against. Pending
                // streaming text stays in assistant_line_buf for whoever
                // legitimately pushes a new row next.
                //
                // Bug seen in 5-8 atomgr session: batch 2 had two bash
                // calls; assistant_line_buf had leftover streamed text
                // ("工具响应持续被截断"-style prose from prior turn). The
                // first ToolCallResult flushed that text → push_body_row
                // → live_group=None → both children's updates silent
                // no-opped. Visual: children stuck without `→ N lines`,
                // user (and model) thought tool results were truncated.

                // Resolve via the active live-group. Three guards:
                // 1. live_group still active (no foreign push happened)
                // 2. batch_id matches (defensive — shouldn't ever
                //    mismatch, but guard against event-order glitches)
                // 3. call_id is in the child map
                // Any miss = silent no-op; the model still got the full
                // ToolResult through the conversation, only the visual
                // ✓ light-up is dropped.
                let group = match self.live_group.as_ref() {
                    Some(g) if g.batch_id == batch_id => g.clone(),
                    _ => return,
                };
                let row_idx = match group.child_indices.get(&call_id) {
                    Some(&i) => i,
                    None => return,
                };

                let muted = self.style_for(Role::Muted);
                let new_row = build_one_row(&new_text, &muted, self.screen.width());

                // Update in-memory.
                if let Some(slot) = self.body_lines.get_mut(row_idx) {
                    *slot = new_row.clone();
                }

                // Compute terminal row position. body_bottom_row is the
                // bottom of the visible body strip; the live-group
                // children sit just above it. body_lines maps to
                // terminal rows from `body_bottom - (len-1)` upwards.
                self.ensure_scroll_region();
                let bottom = self.body_bottom_row();
                if bottom == 0 {
                    return;
                }
                let n = self.body_lines.len();
                let offset_from_bottom = (n - 1).saturating_sub(row_idx);
                if (bottom as usize) <= offset_from_bottom {
                    // Row has scrolled past the visible body strip
                    // into native scrollback — can't rewrite.
                    return;
                }
                let target_row = (bottom as usize) - offset_from_bottom;
                let seq = format!("\x1b[{};1H\x1b[K", target_row);
                let _ = self.out.write_all(seq.as_bytes());
                let bytes = serialize_row(&new_row);
                let _ = self.out.write_all(&bytes);
            }
            UiLine::ToolGroupSummary { text } => {
                self.flush_assistant_remainder();
                // Terminal default fg, NOT bold — distinguishable from
                // the muted children (which apply faint), but quieter
                // than the bold header. Three-tier emphasis: bold
                // header → plain summary → faint children. Was
                // bold-bright-white before; same iTerm2-light invisible
                // bug as the header (see header_style comment above for
                // the full rationale and screenshot).
                let style = self.style_for(Role::Secondary);
                let row = build_one_row(&text, &style, self.screen.width());
                self.push_body_row(row);
            }
            UiLine::ToolCall { name, detail } => {
                self.mark_message(crate::render::MarkKind::ToolCall);
                self.last_mark_was_assistant = false;
                self.flush_assistant_remainder();
                let muted = self.style_for(Role::Muted);
                let tool_name_style = self.style_bold(Role::ToolName);
                let safe_name = scrub_controls(&name);
                let safe_detail = scrub_controls(&detail);
                let body_str = if safe_detail.is_empty() {
                    safe_name.clone()
                } else {
                    format!("{}({})", safe_name, safe_detail)
                };
                // Safety cap: prevent degenerate bodies (e.g. multi-KB bash
                // commands) from producing hundreds of terminal lines.
                let body_str = truncate_body_str(&body_str, 500);
                // only NAME is bolded; retained uses a uniform style
                // for the tool-call line (acceptable in Phase 4,
                // tightens in Phase 5/6).
                let _ = muted;
                // ● (U+25CF, Geometric Shapes block) replaces the
                // earlier ▸ (U+25B8). ▸ ships in Cascadia Code / SF
                // Mono but is missing from Consolas / NSimSun /
                // legacy conhost defaults — Windows users saw the
                // tool-call row prefixed by `□` tofu (screenshot
                // bug report). ● has near-universal monospace
                // coverage, same reason state.tick_spinner picked
                // half-moons over Braille (state.rs:528-544). Bonus:
                // unifies the visual anchor with the parallel-batch
                // header (also ●), matching Claude Code's single-glyph
                // model for tool-call entries.
                self.push_body_prefixed(
                    "● ",
                    &self.style_for(Role::Muted),
                    &body_str,
                    &tool_name_style,
                );
            }
            UiLine::ToolResult { success, summary } => {
                self.mark_message(crate::render::MarkKind::ToolResult);
                self.last_mark_was_assistant = false;
                self.flush_assistant_remainder();
                // Defense in depth: if the event loop didn't send
                // ToolCallCommit before this Result (error path /
                // merge collapse), freeze the in-flight row now so
                // the upcoming `⎿ ...` body push doesn't itself become
                // the next animation target on the next spinner tick.
                // Use commit_inflight_tool for proper line wrapping
                // (see method doc).
                self.commit_inflight_tool();
                // Style policy (header line of a failure body):
                //   * `Error: ...` — bold red. Tool-dispatch failures
                //     (bad JSON args, unknown tool name, etc.) are real
                //     bugs that need attention.
                //   * `[elapsed: ...exit: N...]` — bold yellow. Bash
                //     exit-code failures are frequently recovered by
                //     the agent on the next turn (e.g. `git push`
                //     rejected → next turn `git pull --rebase &&
                //     git push`). Painting them red made transient
                //     hiccups visually identical to real failures.
                // Continuation lines (and success bodies) — default fg.
                //
                // Why split header vs continuation: when an edit_file
                // error includes quoted code (e.g. "Partial match at
                // lines 760-779" + actual file lines), painting the
                // whole block red made it visually identical to a Diff
                // block. Header keeps the urgency signal; body reverts
                // to default fg so quoted code reads like normal output.
                // Three style buckets:
                //   * summary_style — line 0 of a success body, e.g.
                //     `⎿ [elapsed: 0.0s, exit: 0] (4 lines)`. Muted gray
                //     because it's per-call metadata, visually
                //     subordinate to assistant text and tool-call
                //     headers above.
                //   * continuation_style — line ≥ 1 of any body and any
                //     line of multi-line success output. Default fg so
                //     quoted code (edit_file errors) and stderr (bash
                //     failure body) stay readable.
                //   * error_header / warn_header — line 0 of a failure
                //     body, see B-discriminated logic below.
                let summary_style = self.style_for(Role::Muted);
                let continuation_style = self.style_for(Role::Secondary);
                let error_header = self.style_bold(Role::Error);
                let warn_header = self.style_bold(Role::Warning);
                let safe = scrub_controls(&summary);
                // Discriminate before `safe` is moved into body_str.
                // Bash exit-code failures always start with the
                // `format_exit_marker` prefix from bash.rs:578.
                let is_exit_code_failure = !success && safe.starts_with("[elapsed:");
                let body_str = if success {
                    safe
                } else {
                    format!("✗ {}", safe)
                };
                // Align the `└` glyph with the `B` of the `Bash` (or
                // any tool name) in the row above: the tool-call row is
                // `● Bash(...)` with `●` at col 0 and the tool name at
                // col 2, so the result prefix `"  └ "` (2 spaces +
                // glyph + space) lands `└` at col 2 — visually anchored
                // under the tool name. Width reserves PAD_COL for
                // the right gutter + 4 for the prefix `"  └ "`. Was
                // `⎿` (U+23BF, dental symbols block) but Cascadia Code
                // and other Windows monospace defaults render it as a
                // backslash-shaped fallback glyph (user screenshot
                // showed `\` instead of corner). `└` (U+2514, Box
                // Drawing block) ships in every monospace font.
                let row_w = (self.screen.width() as usize).saturating_sub(PAD_COL + 4);
                // Muted (dim gray) for the result prefix — visually subordinate
                // to the tool-call header above (● ToolName).
                let prefix_style = self.style_for(Role::Muted);
                // `└` is a leaf marker for the whole result block, not
                // a per-line bullet — emit it on the FIRST visual row
                // only. Continuation rows (both wrap chunks of one
                // physical line and subsequent `\n`-separated lines)
                // use 4 spaces, same column width as `"  └ "`, so the
                // text stays aligned under the head text.
                let mut first_visual = true;
                for (line_idx, phys) in body_str.split('\n').enumerate() {
                    // First physical line of a failure body is the
                    // header. Wrapped continuation chunks of that same
                    // physical line stay header-styled (a long error
                    // message like "✗ no rows matched: ...stuff..."
                    // shouldn't fade to default mid-sentence).
                    let line_style = if line_idx == 0 {
                        if !success {
                            if is_exit_code_failure {
                                &warn_header
                            } else {
                                &error_header
                            }
                        } else {
                            &summary_style
                        }
                    } else {
                        &continuation_style
                    };
                    for chunk in crate::width::wrap_line_to_width(phys, row_w.max(1)) {
                        let mut row = Vec::new();
                        let prefix = if first_visual { "  └ " } else { "    " };
                        push_str_cells(&mut row, prefix, &prefix_style);
                        push_str_cells(&mut row, &chunk, line_style);
                        self.push_body_row(row);
                        first_visual = false;
                    }
                }
                // No trailing spacer — tool chains stay compact. A
                // following assistant paragraph provides its own
                // breathing room via a single blank line at most
                // (see `push_markdown_body`'s blank-run collapse).
            }
            UiLine::DiffLine { added, text } => {
                let style = self.style_for(if added {
                    Role::DiffAdd
                } else {
                    Role::DiffRemove
                });
                let sign = if added { '+' } else { '-' };
                let body = format!("       {} {}", sign, scrub_controls(&text));
                self.push_body_text(&body, &style);
            }
            UiLine::DiffBlock(entries) => {
                for entry in &entries {
                    let style = self.style_for(if entry.added {
                        Role::DiffAdd
                    } else {
                        Role::DiffRemove
                    });
                    let sign = if entry.added { '+' } else { '-' };
                    let body = format!("       {} {}", sign, scrub_controls(&entry.text));
                    self.push_body_text(&body, &style);
                }
            }

            // ── body: approval / errors / command output ──
            UiLine::ApprovalPrompt { tool, detail } => {
                let warn = self.style_bold(Role::Warning);
                let plain = CellStyle::default();
                let chip = |c: Color| CellStyle {
                    fg: Some(c),
                    bold: true,
                    reverse: true,
                    faint: false,
                };
                let chip_y = chip(Color::Green);
                let chip_a = chip(Color::Cyan);
                let chip_n = chip(Color::Red);

                // Build tool label so user knows which specific action
                // they're approving (issue #439: parallel batch approvals
                // showed identical prompts with no way to tell which file).
                let tool_label = if detail.is_empty() {
                    format!("{}: ", tool)
                } else {
                    format!("{}({}): ", tool, detail)
                };

                let waiting = t(Msg::ApprovalWaitingLabel);
                let prefix_w = crate::width::display_width(&waiting);
                let cont_pad: String = " ".repeat(prefix_w);

                let allow = t(Msg::ApprovalAllow);
                let always = t(Msg::ApprovalAlways);
                let deny = t(Msg::ApprovalDeny);

                // Build the Y/A/N chips cells once — reused whether
                // we place them inline or on a separate line.
                let mut chips_cells: Vec<Cell> = Vec::new();
                push_str_cells(&mut chips_cells, " Y ", &chip_y);
                push_str_cells(&mut chips_cells, &allow, &plain);
                push_str_cells(&mut chips_cells, " A ", &chip_a);
                push_str_cells(&mut chips_cells, &always, &plain);
                push_str_cells(&mut chips_cells, " N ", &chip_n);
                push_str_cells(&mut chips_cells, &deny, &plain);
                let chips_width: usize = chips_cells.iter().map(|c| c.width as usize).sum();

                // Build the label rows, then decide: if the last label
                // row + chips fit within the screen width, append chips
                // inline (issue #454). Otherwise, emit chips on a
                // separate line so they remain visible.
                let safe_tool_label = crate::sanitize::scrub_controls(&tool_label);
                let mut prefixed_rows = self.build_prefixed_rows(&waiting, &warn, &safe_tool_label, &warn);
                let screen_w = self.screen.width() as usize;
                let last_row_w: usize = prefixed_rows
                    .last()
                    .map(|r| r.iter().map(|c| c.width as usize).sum())
                    .unwrap_or(0);

                if last_row_w + chips_width <= screen_w {
                    // Everything fits on one line — append chips directly
                    // after the label.  issue #454: users reported that
                    // splitting into two lines was unnecessary when the
                    // terminal is wide enough.
                    if let Some(last_row) = prefixed_rows.last_mut() {
                        last_row.extend(chips_cells);
                    }
                    for row in prefixed_rows {
                        self.push_body_row(row);
                    }
                } else {
                    // Label too long — keep chips on a separate line so
                    // they remain visible even when the label wraps.
                    for row in prefixed_rows {
                        self.push_body_row(row);
                    }
                    let mut chips_row = Vec::new();
                    push_str_cells(&mut chips_row, &cont_pad, &plain);
                    chips_row.extend(chips_cells);
                    self.push_body_row(chips_row);
                }
            }
            UiLine::Error(msg) => {
                let err_style = self.style_for(Role::Error);
                let safe = scrub_controls(&msg);
                let body = t(Msg::ErrorPrefix { msg: &safe });
                self.push_body_text(&body, &err_style);
            }
            UiLine::Warning(msg) => {
                // Yellow advisory — distinct from Error (red) so users
                // can tell "noticed something" from "turn died". Renders
                // with a `!` glyph + bold yellow body. Always-visible:
                // we deliberately don't dim it because the whole point
                // is to put a truncating-proxy or similar provider
                // pathology in front of the user immediately.
                let warn_style = CellStyle {
                    fg: Some(crossterm::style::Color::Yellow),
                    bold: true,
                    ..CellStyle::default()
                };
                let body = format!("! {}", scrub_controls(&msg));
                self.push_body_text(&body, &warn_style);
            }
            UiLine::CommandOutput(text) => {
                // CommandOutput is trusted internal text — let SGR
                // through the sanitizer so colour / bold / faint
                // attributes survive (e.g. the `/codingplan` red
                // locked-model row). `push_body_text_sgr` parses
                // those escapes into `CellStyle` mutations so the
                // cell pipeline renders the same colours alt_screen
                // and plain do.
                let safe = crate::sanitize::scrub_controls_keep_sgr(&text);
                self.push_body_text_sgr(&safe);
            }
            UiLine::ImageAttachment(n) => {
                // `└` at col 2, under the `[` of `[Image #N]` in the
                // user-message echo above. push_body_text auto-prefixes
                // PAD_COL (2 spaces), so emitting "└ [Image #N]" lands
                // the glyph at col 2. Muted style — visually
                // subordinate to the user message it's anchoring.
                //
                // Tight grouping: `UiLine::User` already wrote a trailing
                // blank spacer to the terminal (LF + EL at body_bottom)
                // and pushed an empty row to body_lines. To make the
                // attachment sit flush under the user message we have to
                // physically REPLACE that visible blank row, not just
                // pop it from memory — popping body_lines leaves the LF
                // already in scrollback and the gap on screen.
                //
                // Mirror the `clear_live_spinner` pattern (see line
                // ~1167): pop body_lines, EL-erase the row at
                // body_bottom, then arm `skip_body_scroll_count` so the
                // next push_body_row overwrites in-place (no LF) instead
                // of scrolling. After the attachment row, push a fresh
                // trailing blank so the next turn's content still has
                // paragraph separation.
                if self.body_lines.last().map_or(false, |r| r.is_empty()) {
                    self.body_lines.pop();
                    self.ensure_scroll_region();
                    let bottom = self.body_bottom_row();
                    if bottom > 0 {
                        let seq = format!("\x1b[{};1H\x1b[K", bottom);
                        let _ = self.out.write_all(seq.as_bytes());
                    }
                    self.skip_body_scroll_count = self.skip_body_scroll_count.saturating_add(1);
                }
                let body = format!("└ [Image #{}]", n);
                self.push_body_text(&body, &self.style_for(Role::Muted));
                self.push_body_row(Vec::new());
            }
            UiLine::VisionPreprocessSuccess { msg, model } => {
                // `{msg}  ` in default text style; `{model}` in Muted
                // (gray) so the model identity reads as metadata, not
                // as part of the success sentence. push_body_prefixed
                // handles the two styles in a single visual line and
                // continues onto wrapped rows with the prefix's display
                // width as continuation pad.
                //
                // Trailing blank: without it the next event's row (e.g.
                // `● Pondering…` spinner or assistant text) butts right
                // up against the success notice — user reported it felt
                // too cramped. The blank lets the success line breathe
                // as its own paragraph.
                let default_style = CellStyle::default();
                let muted_style = self.style_for(Role::Muted);
                let prefix = format!("{msg}  ");
                self.push_body_prefixed(&prefix, &default_style, &model, &muted_style);
                self.push_body_row(Vec::new());
            }
        }
        // Phase 5: widget state updated → mark frame dirty. No
        // paint, no emit. The event loop's 5ms tick (via
        // flush_deferred) will coalesce any further state
        // changes that arrive in the same window into a single
        // paint+emit pass.
        self.dirty = true;
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn pop_approval_prompt(&mut self) {
        // The approval prompt spans one or more body rows:
        //   - When label + chips fit on one line: a single row
        //     starting with '▶' containing both the label and
        //     the Y/A/N chips.
        //   - When the label is long: 1+ label rows (first starts
        //     with '▶', continuation rows start with spaces) plus
        //     1 chips row (also starting with spaces).
        // We need to pop all of them. Strategy: walk backwards
        // from the tail, popping every row until we find the ▶
        // header row (which we also pop). Other symbol rows hold
        // '●' (tool call) or '❯' (user turn) at col 0 — distinct
        // glyphs — so the first ▶ we encounter must be ours.
        // Safe because the agent doesn't append further body rows
        // between `ApprovalNeeded` and the user's Y/A/N reply.
        let mut popped_count: u16 = 0;
        loop {
            let action = match self.body_lines.last() {
                None => break,
                Some(last) => last.get(0).map(|c| c.ch),
            };
            match action {
                // ▶ header: pop it and stop (we've found the start).
                Some('▶') => {
                    self.body_lines.pop();
                    popped_count = popped_count.saturating_add(1);
                    break;
                }
                // Space-padded continuation / chips row: pop and keep going.
                Some(' ') => {
                    self.body_lines.pop();
                    popped_count = popped_count.saturating_add(1);
                }
                // Any other glyph (● tool-call, ❯ user turn, etc.):
                // not part of the approval block — stop without popping.
                _ => break,
            }
        }
        if popped_count == 0 {
            return;
        }
        // Physically wipe the popped rows for instant visual feedback
        // on Y/A/N. The popped rows sat at the BOTTOM of the body
        // region — terminal rows `bottom - popped_count + 1 ..= bottom`
        // (1-indexed). Erase them row-by-row with `\x1b[K` (EL).
        //
        // Why per-row EL and not `\x1b[J` (ED from cursor): the cursor
        // sits at `bottom` (the LAST popped row), and `\x1b[J` erases
        // FROM cursor TO end-of-screen — i.e. that one body row plus
        // every footer row below it. That wipes the input box / top
        // rule / status bar from the terminal. The cell-diff cache
        // (`self.screen.prev_cells`) still holds the prior footer
        // content, so the next `paint_footer` → `render_diff` produces
        // an empty patch (cells == prev_cells, no diff) and the
        // footer never gets redrawn — user sees "input box vanished
        // after approving a tool". EL is row-local, never touches the
        // footer area, and leaves prev_cells consistent. Then flag the
        // next body emit to overwrite in place (no scroll) so
        // `⎿ result` lands directly below the `● Tool` row with no
        // gap.
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            // Erase the popped body rows (may span multiple terminal
            // lines). Use per-row \x1b[K instead of \x1b[J to avoid
            // erasing the footer rows below the body strip.
            // screen.prev_cells still holds the old footer content,
            // so without invalidation the next render_diff() would
            // see identical prev/current footer cells and skip the
            // repaint — leaving the footer permanently blank.
            // invalidate() below ensures the next flush_deferred()
            // emits a full repaint of every non-blank cell.
            let start_row = bottom.saturating_sub(popped_count - 1).max(1);
            let mut seq = String::with_capacity((bottom - start_row + 1) as usize * 8);
            use std::fmt::Write as _;
            for row in start_row..=bottom {
                let _ = write!(seq, "\x1b[{};1H\x1b[K", row);
            }
            let _ = self.out.write_all(seq.as_bytes());
            let _ = self.out.flush();
            self.skip_body_scroll_count = popped_count;
            self.screen.invalidate();
        }
        self.dirty = true;
    }

    fn refresh_welcome_banner(&mut self, model: &str, working_dir: &str) {
        // Body rows are written directly to the terminal during
        // push_body_row — paint_frame only repaints the footer, so a
        // body_lines edit alone doesn't change the bytes already
        // on-screen. To make the new model/working_dir visible we:
        //   1. update the cached banner + splice body_lines, and
        //   2. compute the terminal-row position of each welcome line
        //      that's still in the viewport (anything above viewport
        //      top has already entered native scrollback and is no
        //      longer reachable), then CUP+EL+write each row.
        // Cursor is saved/restored via DECSC/DECRC so the surgical
        // update doesn't disturb whatever the active footer/spinner
        // path expects on its next paint.
        if self.welcome_banner.is_none() {
            return;
        }
        let model_scrubbed = scrub_controls(model);
        let wd_scrubbed = scrub_controls(working_dir);
        self.welcome_banner = Some((model_scrubbed, wd_scrubbed));
        self.reflow_welcome_prefix();

        let bottom = self.body_bottom_row() as usize;
        if bottom == 0 || self.welcome_line_count == 0 {
            return;
        }
        let n = self.body_lines.len();
        if n == 0 {
            return;
        }
        // body_lines tail is bottom-anchored: body_lines[i] sits at
        // terminal row `bottom - n + i + 1` (1-indexed). Rows whose
        // computed position would be <= 0 are already in scrollback.
        let mut seq: Vec<u8> = Vec::with_capacity(self.welcome_line_count * 64);
        seq.extend_from_slice(b"\x1b7");
        let mut wrote = false;
        for i in 0..self.welcome_line_count.min(n) {
            // Saturating math: avoid underflow when n > bottom and i
            // falls in the off-screen prefix. We *want* the result to
            // be 0 in that case so the row is skipped below.
            let abs = (bottom + i + 1).checked_sub(n).unwrap_or(0);
            if abs == 0 {
                continue;
            }
            use std::io::Write as _;
            let _ = write!(&mut seq, "\x1b[{};1H\x1b[K", abs);
            let bytes = serialize_row(&self.body_lines[i]);
            seq.extend_from_slice(&bytes);
            wrote = true;
        }
        seq.extend_from_slice(b"\x1b8");
        if wrote {
            let _ = self.out.write_all(&seq);
            let _ = self.out.flush();
            // Cells on those rows now hold the new content — invalidate
            // the diff cache so the next frame doesn't decide the row
            // is unchanged based on the stale snapshot.
            self.screen.invalidate();
        }
        self.dirty = true;
    }

    fn shutdown(&mut self) {
        // Drain any pending frame before exit so the user sees the
        // latest widget state (typically a final prompt or an error
        // line) rather than a frame that dirty-flagged too late.
        if self.dirty {
            self.paint_frame();
            let bytes = self.screen.render_diff();
            let _ = self.out.write_all(&bytes);
            self.dirty = false;
        }
        self.promote_visible_body_to_scrollback();
        // Be defensive: re-enable autowrap, release any DECSTBM, then
        // wipe the visible viewport and home the cursor. Without the
        // wipe, the welcome banner + input box survive as garbage that
        // the shell's new prompt overwrites from the top, leaving the
        // bottom half visible.
        //
        // Per-row CUP+EL instead of `\x1b[2J` for the same reason as
        // `reset()` / `on_resize()` — iTerm2 3.5+ ignores ED under
        // certain states (see `reset()` rationale). EL is row-local
        // and unambiguous. Scrollback is preserved either way.
        //
        // Also force-restore cursor visibility — if we exit while a
        // spinner is hidden (e.g. SIGINT mid-turn), DECTCEM off would
        // persist into the parent shell and break their prompt cursor.
        let _ = self.out.write_all(b"\x1b[?25h\x1b[?7h\x1b[r");
        let h = self.screen.height() as usize;
        let mut seq = String::with_capacity(h * 8 + 8);
        for row in 1..=h {
            use std::fmt::Write;
            let _ = write!(seq, "\x1b[{};1H\x1b[K", row);
        }
        seq.push_str("\x1b[H");
        let _ = self.out.write_all(seq.as_bytes());
        self.scroll_region_bottom = None;
        let _ = self.out.flush();
    }

    fn reset(&mut self) {
        // Terminal-side wipe + full state reset. `body_lines` is
        // also dropped so post-reset the screen truly starts clean
        // (old transcript stays in the terminal's own scrollback).
        //
        // Why per-row CUP+EL instead of `\x1b[2J`: ED behaviour is
        // inconsistent across terminals — iTerm2 3.5+ was reported
        // to leave pre-reset rows visible after `\x1b[2J` (trace
        // shows `Ack Reset` fires and body_lines is cleared, but
        // the old assistant response + Done separator + user echo
        // stayed on screen while the freshly re-rendered welcome
        // sat below them, leaving `/session` to produce a torn
        // layout). ED also interacts badly with DECSTBM on some
        // builds and can promote visible rows to scrollback rather
        // than clearing. EL (`\x1b[K`) is row-local with no scroll
        // or scrollback semantics, so a CUP+EL per row is
        // unambiguous everywhere (same technique as
        // `ensure_scroll_region`'s resize path).
        //
        // Release DECSTBM first so EL isn't constrained by the
        // prior scroll region.
        let _ = self.out.write_all(b"\x1b[r");
        let h = self.screen.height() as usize;
        let mut seq = String::with_capacity(h * 8 + 8);
        for row in 1..=h {
            use std::fmt::Write;
            let _ = write!(seq, "\x1b[{};1H\x1b[K", row);
        }
        seq.push_str("\x1b[H");
        let _ = self.out.write_all(seq.as_bytes());
        self.screen = Screen::new(self.screen.width(), self.screen.height());
        self.body_lines.clear();
        self.assistant_line_buf.clear();
        self.md_state.reset();
        self.last_painted_footer_rows = 0;
        self.scroll_region_bottom = None;
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
        // Position cursor at the top of where the footer (input box +
        // status + menu) used to be, then clear from there to end of
        // screen. Without this, cursor stays wherever the last paint
        // left it — usually inside the footer area — and the child's
        // first stdout write lands ON TOP of footer rows, with later
        // writes scrolling existing body content up through the
        // overlap. Symptom: `/login`'s OAuth URL printed at row 1
        // overlapping prior scrollback ("Press ESC to cancelh lines?"
        // — our line glued onto an old conversation row).
        //
        // Sequence: release DECSTBM, CUP to (body_bottom+1, col 1),
        // ED 0 (cursor → end of screen), enable autowrap. After this
        // the child writes into a clean rectangle below the body,
        // and as it produces more lines the terminal scrolls naturally
        // (no scroll region active, autowrap on) — which is exactly
        // the cooked-mode shell experience users expect.
        let body_bottom = self.body_bottom_row();
        let position_row = body_bottom.saturating_add(1);
        let seq = format!("\x1b[r\x1b[{};1H\x1b[J\x1b[?7h", position_row);
        let _ = self.out.write_all(seq.as_bytes());
        self.scroll_region_bottom = None;
        // Footer is wiped — record that so the next paint after
        // resume doesn't try to diff against stale footer state.
        self.last_painted_footer_rows = 0;
        let _ = self.out.flush();
        // Pop Kitty keyboard enhancement flags if they were pushed at
        // startup. Without this, the child (OAuth browser output, a
        // shell prompt) runs in a terminal whose key-reporting mode
        // was modified by us — and on some terminals the non-standard
        // CSI u sequences bleed through as unexpected bytes on stdin
        // that the cooked-mode child process then echoes back as
        // gibberish. `execute!` is best-effort — terminals that never
        // accepted the push silently ignore the pop.
        if self.caps.tty {
            let _ = execute!(self.out, PopKeyboardEnhancementFlags);
        }
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
        // Re-push Kitty keyboard enhancement flags (mirror of the pop in
        // suspend_for_external, and the initial push in TerminalGuard).
        // Without this, post-OAuth the terminal is in a different
        // key-reporting mode than we initialised with — autorepeat stops
        // coming as `Repeat`, Shift+Enter stops carrying SHIFT, and any
        // other logic that depended on CSI u event types silently
        // degrades. Same flag set as `TerminalGuard::activate`.
        if self.caps.tty {
            let _ = execute!(
                self.out,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            );
        }
        // Wipe terminal + invalidate Screen + reset region state so
        // the next widget draw is a cold-start full repaint and the
        // next body emit resets DECSTBM. Scrollback is preserved.
        //
        // Per-row CUP+EL instead of `\x1b[2J` for the same reason as
        // `reset()` / `on_resize()` — iTerm2 3.5+ ignores ED under
        // certain states, which after resume would leave the external
        // process's output (shell, OAuth browser messages) overlaid
        // with atomcode's re-painted UI.
        let h = self.screen.height() as usize;
        let mut seq = String::with_capacity(h * 8 + 8);
        for row in 1..=h {
            use std::fmt::Write;
            let _ = write!(seq, "\x1b[{};1H\x1b[K", row);
        }
        seq.push_str("\x1b[H");
        let _ = self.out.write_all(seq.as_bytes());
        self.screen.invalidate();
        self.scroll_region_bottom = None;
        let _ = self.out.flush();
        // Re-emit body tail so the view matches `body_lines` again.
        // Cold-start the region by cloning the tail first (avoid the
        // borrow clash with `emit_body_line_inner(&mut self, ...)`).
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            let tail: Vec<Vec<Cell>> = {
                let n = self.body_lines.len().min(bottom as usize);
                self.body_lines[self.body_lines.len() - n..]
                    .iter()
                    .cloned()
                    .collect()
            };
            // Set region up front so each LF scrolls within the body
            // strip rather than the whole viewport.
            let _ = write!(self.out, "\x1b[1;{}r", bottom);
            self.scroll_region_bottom = Some(bottom);
            for row in &tail {
                self.emit_body_line_inner(row, bottom);
            }
        }
        let _ = self.out.flush();
    }

    fn flush_deferred(&mut self) {
        // The coalesce point. Called every 5ms by the event loop
        // tick. If widget state has changed since the last tick,
        // paint one full frame, diff it against the previous
        // frame, and emit the patch stream. Multiple `render()`
        // calls in the same 5ms window are absorbed into a single
        // paint here.
        if self.dirty {
            let t0 = std::time::Instant::now();
            let footer_rows = self.current_footer_rows();
            // Track footer_rows for diagnostic / resize code paths.
            // We DON'T call `screen.invalidate()` here — invalidate
            // blanks prev_cells, so the diff sees "blank → blank"
            // for every row whose new cells happen to be blank and
            // skips the emit. That's wrong whenever the previous
            // frame had non-blank content at those rows (e.g. menu
            // close: welcome moves down a few rows, leaving the
            // top rows of the old welcome position with no erase
            // patch against them → ghost text on screen). Letting
            // the real prev→current diff run produces the correct
            // erase patches naturally.
            if footer_rows != self.last_painted_footer_rows {
                self.last_painted_footer_rows = footer_rows;
            }
            let has_status = !self.status.model.is_empty()
                || !self.status.cwd.is_empty()
                || self.status.hint.is_some();
            let middle_rows = footer_rows.saturating_sub(
                1 /* spinner */
                + 1 /* top rule */
                + 1 /* bot rule */
                + self.menu.as_ref().map(|m| m.items.len().min(4)).unwrap_or(0)
                + if has_status { 1 } else { 0 },
            );
            let menu_rows = self
                .menu
                .as_ref()
                .map(|m| m.items.len().min(4))
                .unwrap_or(0);
            let buf_display_w = crate::width::display_width(&self.input_buf);
            self.paint_frame();
            let bytes = self.screen.render_diff();
            let emit_len = bytes.len();
            // Chunked emit: Mac Terminal.app has been observed to drop
            // bytes mid-sequence when a single write carries ~1KB+ of
            // mixed CSI+SGR+UTF-8 — the bot_rule "shortens" bug. Split
            // into 512-byte chunks with a flush in between so each
            // chunk reaches the terminal as its own parse cycle.
            // Trade-off: +N syscalls per frame. Typical frame 50-200B
            // fits in one chunk; only wrap / menu / cold-start frames
            // (~1-2KB) incur 2-4 chunks. Still single-digit ms.
            const CHUNK: usize = 512;
            let mut offset = 0;
            while offset < bytes.len() {
                let end = (offset + CHUNK).min(bytes.len());
                let _ = self.out.write_all(&bytes[offset..end]);
                if end < bytes.len() {
                    // Inter-chunk flush; the final-chunk flush is at
                    // the end of this method.
                    let _ = self.out.flush();
                }
                offset = end;
            }
            self.dirty = false;
            // Diagnostic: count how many cells on the bot_rule row
            // (screen_h - 2, 0-indexed) actually hold '─'. bot_rule
            // sits at a constant absolute row regardless of middle
            // row count — if this goes to zero while middle_rows > 1,
            // some path (body overwrite, diff skip, draw_row truncate)
            // is blanking out the rule.
            let screen_h = self.screen.height() as usize;
            let bot_rule_row = screen_h.saturating_sub(2);
            let bot_rule_dashes = self
                .screen
                .prev_cells_for_test()
                .get(bot_rule_row)
                .map(|r| r.iter().filter(|c| c.ch == '─').count())
                .unwrap_or(0);
            crate::tuix_trace!(
                "FOOT",
                "paint screen={}x{} rows=footer{}(mid={} menu={}) body={} buf_w={} emit={}B botrule_row={} botrule_dashes={} dur={}µs",
                self.screen.width(),
                self.screen.height(),
                footer_rows,
                middle_rows,
                menu_rows,
                self.body_lines.len(),
                buf_display_w,
                emit_len,
                bot_rule_row,
                bot_rule_dashes,
                t0.elapsed().as_micros()
            );
        }
        let _ = self.out.flush();
    }

    fn scroll_body(&mut self, delta: i32) {
        let body_height = self.body_bottom_row() as usize;
        let total = self.body_lines.len();
        let max_top = total.saturating_sub(body_height);
        if max_top == 0 {
            // Nothing to scroll; stay sticky.
            self.sticky_bottom = true;
            self.view_mode = false;
            return;
        }
        let current_top = if self.sticky_bottom {
            max_top
        } else {
            self.viewport_top
        };
        let new_top: usize = if delta < 0 {
            current_top.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (current_top + delta as usize).min(max_top)
        };
        self.viewport_top = new_top;
        self.sticky_bottom = new_top >= max_top;
        let was_view = self.view_mode;
        self.view_mode = !self.sticky_bottom;
        // Trigger paint. When transitioning out of view_mode (was_view=true,
        // view_mode=false), the next paint_body must repaint the body tail
        // without a `\n` scroll (handled in P3.5).
        if was_view != self.view_mode || self.view_mode {
            self.repaint_body_region();
        }
    }

    fn scroll_body_to_top(&mut self) {
        let body_height = self.body_bottom_row() as usize;
        let total = self.body_lines.len();
        if total <= body_height {
            return;
        }
        self.viewport_top = 0;
        self.sticky_bottom = false;
        self.view_mode = true;
        self.repaint_body_region();
    }

    fn scroll_body_to_bottom(&mut self) {
        let was_view = self.view_mode;
        self.viewport_top = self
            .body_lines
            .len()
            .saturating_sub(self.body_bottom_row() as usize);
        self.sticky_bottom = true;
        self.view_mode = false;
        if was_view {
            // Exiting view: repaint body tail without LF (Task 3.5).
            self.repaint_body_region();
        }
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        // No-op if size unchanged. Some terminals fire `Resize` for
        // shape changes that don't actually alter the cell grid (tab
        // toggles, font-size cycles, focus events on multiplexers);
        // the per-row CUP+EL wipe below is visible flicker even when
        // the result would be byte-identical, so skip the work
        // entirely. Pairs with the burst coalescing in
        // `event_loop::handle_input` — together they collapse a
        // window-drag's 30+ same-size tail events into a single paint.
        if cols == self.screen.width() && rows == self.screen.height() {
            return;
        }
        // Terminal-side wipe: resize leaves pre-resize chars at old
        // absolute positions. Use per-row CUP+EL instead of `\x1b[2J`
        // for the same reason as `reset()` — iTerm2 3.5+ has been
        // observed to ignore ED under certain states, leaving the
        // pre-resize welcome + footer on screen while the body
        // repaint below stamps a second copy. EL is row-local and
        // unambiguous across terminals.
        //
        // Release DECSTBM first so EL isn't constrained by the
        // stale (pre-resize) scroll region.
        let _ = self.out.write_all(b"\x1b[r");
        let mut seq = String::with_capacity((rows as usize) * 8 + 8);
        for row in 1..=(rows as usize) {
            use std::fmt::Write;
            let _ = write!(seq, "\x1b[{};1H\x1b[K", row);
        }
        seq.push_str("\x1b[H");
        let _ = self.out.write_all(seq.as_bytes());
        self.scroll_region_bottom = None;
        self.screen.resize(cols, rows);
        // Rebuild the semantic welcome banner against the new width so
        // its right-aligned version/license pair stays adaptive after
        // terminal resize instead of replaying stale gap cells.
        self.reflow_welcome_prefix();
        // Re-emit body tail into the new region so the view matches
        // memory. Set region first so LFs scroll only within body.
        //
        // Cached `body_lines` cells were built against the OLD screen
        // width — after a resize-smaller drag, rows may exceed the new
        // terminal width. `serialize_row` writes every real cell, so
        // overflow would trigger the terminal's own auto-wrap; the
        // wrapped remainder lands on the next row, which on a fresh
        // DECSTBM region is either the footer strip or the next body
        // slot. Symptom the user sees: content shifted by a column and
        // junk in the footer strip. Clip each row to the new width
        // before handing it to `emit_body_line_inner` so we never
        // rely on the terminal to hide our overflow.
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            let screen_w = self.screen.width() as usize;
            let tail: Vec<Vec<Cell>> = {
                let n = self.body_lines.len().min(bottom as usize);
                self.body_lines[self.body_lines.len() - n..]
                    .iter()
                    .map(|row| clip_cells_to_width(row, screen_w))
                    .collect()
            };
            let _ = write!(self.out, "\x1b[1;{}r", bottom);
            self.scroll_region_bottom = Some(bottom);
            // Direct CUP per row instead of `emit_body_line_inner`'s
            // LF-at-bottom scroll. LF inside the DECSTBM `[1, bottom]`
            // region pushes the top row out — and since we just erased
            // every row, that top row is blank. A full tail-repaint
            // would therefore inject `tail.len() - 1` blank rows into
            // scrollback. User symptom: after resizing smaller, the
            // scrollback above the current page fills with empty rows
            // for every resize event. Positioning absolutely with
            // `\x1b[row;1H` skips the scroll entirely and leaves
            // scrollback untouched.
            let n = tail.len() as u16;
            let first_row = bottom.saturating_sub(n) + 1;
            for (i, row) in tail.iter().enumerate() {
                let seq = format!("\x1b[{};1H\x1b[K", first_row + i as u16);
                let _ = self.out.write_all(seq.as_bytes());
                let bytes = serialize_row(row);
                let _ = self.out.write_all(&bytes);
            }
        }
        self.paint_frame();
        self.flush_frame();
        let _ = self.out.flush();
        self.last_painted_footer_rows = self.current_footer_rows();
        self.dirty = false;
    }
}

/// Build a single-line row from `text`, flush-left at col 0, truncated
/// with `…` when the text overflows the screen width. Used by the
/// live-group rendering path (ToolGroupRender header / children /
/// summary, ToolGroupChildUpdate) where each child must be exactly
/// one terminal row so child indices map 1:1 with terminal positions
/// for in-place CUP rewrites.
///
/// Flush-left, no leading PAD_COL: header glyph (●) sits at col 0
/// aligned with the user-message ❯ chevron and the single tool-call
/// ● glyph (push_body_prefixed paths). Children carry a 2-space
/// prefix in their own text (event_loop builds `"  └ Bash(...)"`),
/// so they still indent under the header without extra padding here.
/// The previous PAD_COL leading pad pushed the header glyph to col 2
/// and the children to col 4, breaking visual alignment with the
/// rest of the body which lives at col 0 (user messages, single
/// tool calls).
fn build_one_row(text: &str, style: &CellStyle, screen_w: u16) -> Vec<Cell> {
    let avail = (screen_w as usize).saturating_sub(PAD_COL);
    let safe = scrub_controls(text);
    // Width-aware truncation: CJK glyphs occupy 2 cols each, so a row of
    // 30 汉字 (60 cols) on a 40-col screen must trip truncate and append `…`,
    // not slip past the chars().count() check and leak past the screen edge.
    let truncated = crate::width::truncate_with_ellipsis(&safe, avail.max(1));
    let mut row = Vec::new();
    push_str_cells(&mut row, &truncated, style);
    row
}

/// Truncate `body_str` so its display width is at most `max_cols`,
/// preserving grapheme clusters (never splits a multi-codepoint emoji or
/// a CJK glyph). Appends `… (truncated)` when a cut happened.
///
/// Rendering safeguard against degenerate bodies (e.g. multi-KB bash
/// commands) producing hundreds of terminal lines.
fn truncate_body_str(body_str: &str, max_cols: usize) -> String {
    if crate::width::display_width(body_str) <= max_cols {
        return body_str.to_string();
    }
    let suffix = "… (truncated)";
    let suffix_w = crate::width::display_width(suffix);
    let budget = max_cols.saturating_sub(suffix_w);
    if budget == 0 {
        // Budget too small to fit even one cluster of body + the suffix.
        // Emit just the suffix, capped at max_cols.
        return crate::width::truncate_to_width(suffix, max_cols);
    }
    let head = crate::width::truncate_to_width(body_str, budget);
    format!("{}{}", head, suffix)
}

/// Pluck the metadata suffix (` · 12s` and/or ` · N queued`) out of a
/// spinner label built by `format_spinner_label`. Labels have the
/// shape `{base}{ellipsis}[ · {elapsed}][ · {n} queued]`, so the first
/// ` · ` marks where the base ends and the metadata begins. Returns
/// the slice **including** its leading ` · ` separator so callers can
/// concatenate it directly, or `""` if the label has no metadata yet
/// (no phase clock has ticked).
fn spinner_meta_suffix(label: &str) -> &str {
    label.find(" · ").map(|i| &label[i..]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{EnvView, TerminalCaps};
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    };

    #[test]
    fn ctx_usage_with_known_window_shows_ratio() {
        // The user's actual ask: "10.4k tokens" alone is uninformative —
        // they want to see how close to the limit the context is. With a
        // window, render `used/window tok` so saturation is visible.
        assert_eq!(format_ctx_usage(10_400, 131_000), "10.4k/131k tok");
    }

    #[test]
    fn ctx_usage_keeps_round_window_clean() {
        // 128k window is the common default — render as `128k`, not `128.0k`.
        assert_eq!(format_ctx_usage(50_000, 128_000), "50.0k/128k tok");
    }

    #[test]
    fn ctx_usage_without_window_shows_used_only() {
        // Pre-first-turn / unknown-provider fallback — window unknown.
        // Better to show the count alone than a misleading "/0".
        assert_eq!(format_ctx_usage(10_400, 0), "10.4k tok");
    }

    #[test]
    fn ctx_usage_under_one_thousand_keeps_raw_count() {
        assert_eq!(format_ctx_usage(523, 131_000), "523/131k tok");
        assert_eq!(format_ctx_usage(523, 0), "523 tok");
    }

    #[test]
    fn ctx_usage_non_round_window_rounds_to_nearest_k() {
        // GLM-5.1 endpoint ships a 131_072 window; we display 131k, not 131.072k.
        assert_eq!(format_ctx_usage(50_000, 131_072), "50.0k/131k tok");
    }

    fn caps_with_color() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            term: Some("xterm-256color".into()),
            colorterm: Some("truecolor".into()),
            lang: Some("en_US.UTF-8".into()),
            ..Default::default()
        })
    }

    /// Writer that tallies byte count — for assert-byte-budget tests.
    struct CountingSink(Arc<AtomicU64>);
    impl Write for CountingSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.fetch_add(b.len() as u64, Ordering::Relaxed);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Writer that tracks every individual `write` call — for tests
    /// that assert emit is split into N chunks (Mac Terminal byte-drop
    /// workaround).
    #[derive(Clone)]
    struct ChunkCountingSink {
        chunks: Arc<Mutex<Vec<usize>>>,
    }
    impl Write for ChunkCountingSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.chunks.lock().unwrap().push(b.len());
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn new_chunk_counting(
        w: u16,
        h: u16,
    ) -> (RetainedRenderer<ChunkCountingSink>, Arc<Mutex<Vec<usize>>>) {
        let chunks = Arc::new(Mutex::new(Vec::<usize>::new()));
        let sink = ChunkCountingSink {
            chunks: chunks.clone(),
        };
        let r = RetainedRenderer::with_writer(sink, caps_with_color(), w, h);
        (r, chunks)
    }

    /// Writer that captures the ANSI byte stream — lets us inspect
    /// structure (e.g. "all three wide chars emitted consecutively").
    #[derive(Clone)]
    struct CapturingSink(Arc<Mutex<Vec<u8>>>);
    impl Write for CapturingSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn new_counting(w: u16, h: u16) -> (RetainedRenderer<CountingSink>, Arc<AtomicU64>) {
        let counter = Arc::new(AtomicU64::new(0));
        let sink = CountingSink(counter.clone());
        let r = RetainedRenderer::with_writer(sink, caps_with_color(), w, h);
        (r, counter)
    }

    fn new_capturing(w: u16, h: u16) -> (RetainedRenderer<CapturingSink>, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink(buf.clone());
        let r = RetainedRenderer::with_writer(sink, caps_with_color(), w, h);
        (r, buf)
    }

    /// Phase 7 harness: drain the capture sink's accumulated
    /// ANSI bytes into the virtual terminal so `vterm.cell_at` /
    /// `row_text` / `dump` reflect the post-paint on-screen state.
    /// The sink is left empty afterwards so subsequent renders
    /// accumulate their own bytes for another feed cycle.
    fn drain_into_vterm(buf: &Arc<Mutex<Vec<u8>>>, vterm: &mut crate::test_term::VirtualTerminal) {
        let bytes: Vec<u8> = std::mem::take(&mut *buf.lock().unwrap());
        vterm.feed(&bytes);
    }

    fn sample(c: &Arc<AtomicU64>) -> u64 {
        c.load(Ordering::Relaxed)
    }

    fn status_basic() -> StatusLine {
        StatusLine {
            model: "glm-5".into(),
            cwd: "~/project/atomcode".into(),
            ctx_used: 0,
                ctx_window: 0,
            hint: None,
            mode_indicator: None,
            session_name: None,
        }
    }

    /// Mode indicator (Plan badge) renders BEFORE the model · cwd · tokens
    /// run. Default Build mode (`mode_indicator = None`) keeps the row
    /// unchanged so existing layout / byte-budget tests stay valid.
    #[test]
    fn build_status_row_renders_mode_badge_before_left_run() {
        let (mut r, _counter) = new_counting(80, 24);
        // Force unicode + colors so the brand SGR is reachable; without
        // this the test target (CI sometimes) drops the SGR and we can't
        // distinguish badge cells from body cells.
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let status = StatusLine {
            model: "glm-5".into(),
            cwd: "~/proj".into(),
            ctx_used: 0,
                ctx_window: 0,
            hint: None,
            mode_indicator: Some("PLAN".into()),
            session_name: None,
        };
        let row = r.build_status_row(&status, 60);
        // Concatenate visible chars from the cells. `PAD_COL` of leading
        // spaces, then the badge, then a separator space, then the body.
        let visible: String = row.iter().map(|c| c.ch).collect();
        let trimmed = visible.trim_start();
        assert!(
            trimmed.starts_with("PLAN "),
            "badge must precede the model run; got: {:?}",
            visible
        );
        assert!(
            visible.contains("glm-5"),
            "model name must still appear in the row; got: {:?}",
            visible
        );
    }

    /// Default Build mode produces no badge — row is identical to the
    /// pre-mode-indicator layout. Guards against accidental "PLAN" leak
    /// when no mode is active.
    #[test]
    fn build_status_row_default_mode_emits_no_badge() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let row = r.build_status_row(&status_basic(), 60);
        let visible: String = row.iter().map(|c| c.ch).collect();
        assert!(
            !visible.contains("PLAN"),
            "no mode indicator should produce no PLAN badge; got: {:?}",
            visible
        );
    }

    /// Session-name pill: the top rule must overlay ` {name} ` in
    /// reverse-cyan cells on the right side. Mirrors CC's per-
    /// conversation badge so the user sees which session they're
    /// typing into without opening the picker.
    #[test]
    fn build_top_rule_with_badge_renders_session_name_in_reverse_cyan() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let row = r.build_top_rule_with_badge(60, Some("atomcode加解密"));
        // Skip continuation cells (width 0 placeholders that follow a
        // wide glyph) — they carry `ch = ' '` and would break a naive
        // substring check on a CJK name.
        let visible: String = row.iter().filter(|c| c.width > 0).map(|c| c.ch).collect();
        assert!(
            visible.contains("atomcode加解密"),
            "session name must appear in the top rule cells. got: {:?}",
            visible
        );
        let any_reverse = row.iter().any(|c| c.style.reverse);
        assert!(
            any_reverse,
            "at least one cell of the pill must carry reverse-video style"
        );
    }

    /// `None` session_name keeps the top rule pristine — no reverse
    /// cells, no text overlay. Guards against the badge leaking onto
    /// auto-named or default sessions.
    #[test]
    fn build_top_rule_with_badge_none_emits_plain_rule() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let row = r.build_top_rule_with_badge(60, None);
        assert_eq!(row.len(), 60, "rule width must be preserved");
        assert!(
            row.iter().all(|c| c.ch == '─'),
            "without a session name every cell must be a bare ─"
        );
        assert!(
            row.iter().all(|c| !c.style.reverse),
            "no reverse-video cells allowed when session_name is None"
        );
    }

    /// Overlong names get truncated with `…` so the rule width is
    /// preserved and at least a minimum stretch of ─ stays visible on
    /// the left as a visual anchor for the input box border.
    #[test]
    fn build_top_rule_with_badge_truncates_long_name() {
        let (mut r, _counter) = new_counting(40, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let long = "这是一个非常非常非常非常长的会话名字应当被截断省略";
        let row = r.build_top_rule_with_badge(40, Some(long));
        // Same continuation-cell filter rationale as the badge-render
        // test above: width-0 cells carry ' ' and would obscure the
        // substring assertions on CJK names.
        let visible: String = row.iter().filter(|c| c.width > 0).map(|c| c.ch).collect();
        assert!(
            visible.contains('…'),
            "overlong name must be ellipsised. got: {:?}",
            visible
        );
        assert!(
            !visible.contains(long),
            "full overlong name must NOT appear verbatim. got: {:?}",
            visible
        );
    }

    /// Keystroke steady-state: only the middle row's last cell
    /// changes between frames. AnsiRenderer hit 26 B; retained
    /// should be in the same ballpark. Budget: < 60 B.
    #[test]
    fn retained_keystroke_byte_cost_steady_state() {
        let (mut r, counter) = new_counting(80, 24);
        let status = status_basic();
        // Warm: render one frame so prev_cells matches terminal.
        r.render(UiLine::InputPrompt {
            buf: "h".into(),
            cursor_byte: 1,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let before = sample(&counter);
        for i in 1..=10 {
            let s = "h".repeat(i + 1);
            r.render(UiLine::InputPrompt {
                buf: s.clone(),
                cursor_byte: s.len(),
                menu: None,
                status: status.clone(),
                attachments: Vec::new(),
            });
        }
        r.flush_deferred();
        let avg = (sample(&counter) - before) / 10;
        eprintln!("[RETAINED BYTE] keystroke avg = {} B", avg);
        assert!(
            avg < 60,
            "retained keystroke regressed: avg={} B (budget < 60)",
            avg
        );
    }

    /// Menu open/close: footer height changes 5↔9 → cell-diff must
    /// emit only changed positions. AnsiRenderer hit 880 B at 80
    /// col; retained should match. Budget: < 1000 B.
    #[test]
    fn retained_menu_toggle_byte_cost() {
        let (mut r, counter) = new_counting(80, 24);
        let status = status_basic();
        let items: Vec<(String, String)> = vec![
            ("model".into(), "Switch model".into()),
            ("provider".into(), "Add provider".into()),
            ("session".into(), "New session".into()),
            ("resume".into(), "Resume session".into()),
        ];
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        let before_open = sample(&counter);
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items: items.clone(),
                selected: 0,
                    kind: crate::render::MenuKind::SlashCommand,
            }),
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let open_cost = sample(&counter) - before_open;

        let before_close = sample(&counter);
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let close_cost = sample(&counter) - before_close;

        // Nav: 3 Up/Down changes.
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items: items.clone(),
                selected: 0,
                    kind: crate::render::MenuKind::SlashCommand,
            }),
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let before_nav = sample(&counter);
        for sel in 1..=3 {
            r.render(UiLine::InputPrompt {
                buf: "/".into(),
                cursor_byte: 1,
                menu: Some(MenuPayload {
                    items: items.clone(),
                    selected: sel,
                    kind: crate::render::MenuKind::SlashCommand,
                }),
                status: status.clone(),
                attachments: Vec::new(),
            });
        }
        r.flush_deferred();
        let nav_avg = (sample(&counter) - before_nav) / 3;

        eprintln!(
            "[RETAINED BYTE] menu open={} B, close={} B, nav avg={} B",
            open_cost, close_cost, nav_avg
        );
        assert!(open_cost < 1000, "retained open: {} B", open_cost);
        assert!(close_cost < 1000, "retained close: {} B", close_cost);
        assert!(nav_avg < 300, "retained nav: {} B", nav_avg);
    }

    /// Streaming delta byte cost: scenario mirrors agent_events
    /// emitting `AssistantText` + `StreamingBox` repeatedly. Each
    /// iteration appends a short line to the body + re-paints the
    /// footer spinner. Budget: < 200 B/iteration (AnsiRenderer was
    /// 41 B for streaming-only, but retained pays an extra
    /// full-frame cost for the trailing StreamingBox re-paint).
    #[test]
    fn retained_streaming_delta_byte_cost() {
        let (mut r, counter) = new_counting(80, 24);
        let status = status_basic();
        // Initial spinner footer.
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Thinking".into(),
            status: status.clone(),
            menu: None,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let before_burst = sample(&counter);
        for i in 0..20 {
            r.render(UiLine::AssistantText(format!("line {}\n", i)));
            r.render(UiLine::StreamingBox {
                buf: String::new(),
                cursor_byte: 0,
                frame: "⠹",
                label: "Thinking".into(),
                status: status.clone(),
                menu: None,
                attachments: Vec::new(),
            });
        }
        r.flush_deferred();
        let avg_per_delta = (sample(&counter) - before_burst) / 20;
        eprintln!(
            "[RETAINED BYTE] streaming avg per (delta + box redraw) = {} B",
            avg_per_delta
        );
        assert!(
            avg_per_delta < 250,
            "retained streaming regressed: {} B/iter (budget < 250)",
            avg_per_delta
        );
    }

    /// Phase 5 coalesce contract: N render() calls followed by a
    /// single flush_deferred() must produce exactly ONE emit (or
    /// zero, if nothing visibly changed since the last frame).
    /// Without coalesce, Phase 4 would emit N times. Regression
    /// target: IME burst of 40 chars = 1 terminal repaint, not 40.
    #[test]
    fn retained_coalesce_many_renders_one_emit() {
        let (mut r, counter) = new_counting(80, 24);
        let status = status_basic();
        // Establish initial frame so subsequent diffs are small.
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        let before_burst = sample(&counter);
        // Simulate IME burst: 40 keystrokes in zero time.
        let mut buf = String::new();
        for ch in
            "你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁你是谁".chars()
        {
            buf.push(ch);
            r.render(UiLine::InputPrompt {
                buf: buf.clone(),
                cursor_byte: buf.len(),
                menu: None,
                status: status.clone(),
                attachments: Vec::new(),
            });
        }
        // Zero byte count so far — coalesce should hold every
        // render() as dirty-flag updates only.
        assert_eq!(
            sample(&counter) - before_burst,
            0,
            "render() must not emit bytes before flush_deferred fires"
        );

        // The tick fires → ONE paint+emit covering all 40 state
        // changes at once.
        r.flush_deferred();
        let burst_bytes = sample(&counter) - before_burst;
        eprintln!(
            "[RETAINED BYTE] coalesce: 40 renders + 1 tick = {} B total",
            burst_bytes
        );
        // Upper bound: cold start (first paint after session init)
        // re-emits every non-blank cell + UTF-8 CJK + rule + cursor
        // moves. Budget 1200 B; typical observed ~700 B.
        assert!(
            burst_bytes > 0 && burst_bytes < 1200,
            "coalesce should produce exactly one modest emit: {} B",
            burst_bytes
        );

        // Second tick with no state change → truly zero emit.
        let before_idle = sample(&counter);
        r.flush_deferred();
        let idle_bytes = sample(&counter) - before_idle;
        assert_eq!(idle_bytes, 0, "idle tick should emit 0 bytes");
    }

    /// Regression: user reported that after resizing the terminal
    /// smaller, scrolling up in the terminal revealed many blank rows
    /// above the current page. Root cause: `on_resize` repainted the
    /// body tail via `emit_body_line_inner`, which uses `\n` inside
    /// the DECSTBM `[1, bottom]` region to place each row. Since the
    /// just-cleared top-row of that region gets pushed to scrollback
    /// on every `\n`, a full tail-repaint injected `tail.len() - 1`
    /// blank rows into scrollback for every resize event.
    ///
    /// `on_resize` is a no-op when geometry is unchanged. Some
    /// terminals fire spurious `Resize` events on tab/focus/pane
    /// shuffles where the cell grid doesn't actually change; the
    /// per-row CUP+EL wipe inside `on_resize` is a visible flash even
    /// when the outcome would be byte-identical. Pairs with the
    /// burst-coalesce in `event_loop::handle_input` to collapse a
    /// window-drag's same-size tail into a single paint.
    #[test]
    fn retained_resize_same_size_emits_nothing() {
        let (mut r, buf) = new_capturing(80, 24);
        let status = status_basic();
        r.render(UiLine::User("hi".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let bytes_before = buf.lock().unwrap().len();
        r.on_resize(80, 24);
        let bytes_after = buf.lock().unwrap().len();
        assert_eq!(
            bytes_before, bytes_after,
            "same-size on_resize must not emit any bytes (flicker source)"
        );
    }

    /// Fix: position each tail row with absolute CUP + EL instead of
    /// LF-scrolling, so scrollback is never touched during resize.
    #[test]
    fn retained_resize_does_not_pollute_scrollback_with_blanks() {
        let (mut r, buf) = new_capturing(80, 24);
        let status = status_basic();

        // Seed some body content so there's a tail to re-emit.
        r.render(UiLine::User("first".into()));
        r.render(UiLine::User("second".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Baseline: feed everything so far into the vterm and record
        // how many rows have scrolled off the top.
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        drain_into_vterm(&buf, &mut vterm);
        let baseline_scrollback = vterm.scrollback_len();

        // Now trigger resize-smaller. All bytes emitted by the resize
        // path go to `buf`; feed them alone into the vterm to measure
        // the resize's contribution to scrollback in isolation.
        r.on_resize(60, 16);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let mut vterm_after = crate::test_term::VirtualTerminal::new(60, 16);
        drain_into_vterm(&buf, &mut vterm_after);

        // Scrollback from the RESIZE alone (vterm_after starts fresh).
        // Before the fix, on_resize emitted `tail.len() - 1` blank
        // rows into scrollback; after the fix it must emit zero.
        assert_eq!(
            vterm_after.scrollback_len(),
            0,
            "resize pushed {} rows into scrollback; expected 0 \
             (baseline before resize: {})",
            vterm_after.scrollback_len(),
            baseline_scrollback
        );
    }

    /// Regression: user showed a 5-column CJK table with long cells
    /// overflowing past the terminal's right edge — `flush_aligned_table`
    /// was ignoring terminal width. This test verifies the full pipeline
    /// (streamed assistant text → `render_line_with_width` → body_lines)
    /// keeps every rendered body row within screen width.
    #[test]
    fn retained_wide_table_truncated_to_screen_width() {
        let term_w: u16 = 100;
        let (mut r, _buf) = new_capturing(term_w, 30);
        let status = status_basic();

        let table = "\
| 特性 | 免费版 | 专业版 | 企业版 | 旗舰版 |
|------|--------|--------|--------|--------|
| 价格 | 完全免费，适合个人开发者和学生群体使用 | 每月 $9.9，适合小型团队和独立开发者 | 每月 $49，适合中型企业和专业团队 | 每月 $199，适合大型企业和需要高级功能的用户 |
| 支持语言 | 支持 Python、JavaScript、TypeScript 三种主流编程语言 | 支持所有主流编程语言，包括但不限于 Python、JavaScript、TypeScript、Java、Kotlin、Swift、Rust、Go 等 20+ 种语言 | 支持所有编程语言，无任何限制 | 支持所有已知编程语言 |

尾部文本触发表格 flush。
";
        for line in table.lines() {
            r.render(UiLine::AssistantText(format!("{}\n", line)));
        }
        r.render(UiLine::AssistantLineBreak);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Body rows carry styling + 2-col PAD_COL indent. Strip ANSI and
        // check the display width of each cached body row.
        for (i, row) in r.body_lines.iter().enumerate() {
            let w: usize = row.iter().map(|c| c.width as usize).sum();
            assert!(
                w <= term_w as usize,
                "body row {} has display width {} > terminal {}; \
                 table rendered without width-aware truncation",
                i,
                w,
                term_w
            );
        }
    }

    /// Regression (datalog symptom: the screen filled with ~35 rows of
    /// `<spinner-glyph> Bash(cd /Users/.../cargo metadata...|python3 -c …`
    /// stacking up). Root cause: a wide tool name+detail row, repainted
    /// every spinner tick, would auto-wrap on the bottom row of the
    /// DECSTBM region and the upper portion would scroll up into body
    /// history — accumulating residue.
    ///
    /// Fix (post-merge): `render_inflight_tool` wraps the body via
    /// `push_body_prefixed` so each pushed row fits the terminal width,
    /// AND tracks `inflight_tool_rows` so the next call removes the
    /// previously rendered rows before re-rendering — body_lines no
    /// longer accumulates across ticks.
    #[test]
    fn retained_inflight_tool_row_wraps_and_replaces_in_place() {
        let term_w: u16 = 80;
        let (mut r, _buf) = new_capturing(term_w, 24);
        // A real bash command from the failure datalog — well over 80
        // columns — drives the regression.
        let detail = "cd /Users/yubangxu/project/atomgr && cargo metadata --format-version 1 \
                      2>/dev/null | python3 -c \"import sys,json; d=json.load(sys.stdin); \
                      print([p['name'] for p in d['packages']])\"";
        r.render_inflight_tool("⠋", "bash", detail, "");
        // Every wrapped row must fit the terminal — otherwise DECSTBM
        // auto-wrap on subsequent repaints turns into scroll residue.
        for (i, row) in r.body_lines.iter().enumerate() {
            let w: usize = row.iter().map(|c| c.width as usize).sum();
            assert!(
                w <= term_w as usize,
                "body_lines[{}] width {} exceeds terminal {}",
                i,
                w,
                term_w
            );
        }
        // Simulated spinner ticks: body_lines must not grow — each tick
        // removes the prior inflight rows before re-rendering.
        let after_first = r.body_lines.len();
        for _ in 0..10 {
            r.render_inflight_tool("⠙", "bash", detail, "");
        }
        assert_eq!(
            r.body_lines.len(),
            after_first,
            "body_lines grew across spinner ticks — render_inflight_tool \
             must remove previous inflight rows before re-rendering"
        );
    }

    /// Regression (datalog 2026-05-08_02-39-44 + screenshots 40.png/41.jpeg):
    /// the model emitted ONE `cargo build 2>&1 | tail -5` call that ran
    /// for 39.6s, but the user's terminal ended up with 30+ identical
    /// `▸ Bash(...)` rows stacked in scrollback. Root cause was
    /// `render_inflight_tool` calling `push_body_row` →
    /// `emit_body_line_inner` whose default branch issues a `\n` to
    /// scroll new content into the DECSTBM body region. Each spinner
    /// tick (~80ms) emitted a fresh copy of the inflight row, scrolling
    /// the previous tick's row up — those rows STAY in the terminal's
    /// scrollback even after the renderer truncates them out of
    /// `body_lines`. The pre-existing `retained_inflight_tool_row_*`
    /// test only checked `body_lines.len()`; the actual leak was on
    /// the terminal output stream.
    ///
    /// Fix: when re-rendering on top of a prior inflight render with
    /// matching row count, write each row in-place via cursor-position +
    /// erase-line (no `\n`, no scroll), so the terminal's scrollback
    /// stays clean across ticks. This test captures the output bytes
    /// and asserts their length doesn't blow up — a stream of N ticks
    /// must produce at most O(N) bytes of update sequences, not O(N)
    /// full row scrolls of accumulated content.
    #[test]
    fn retained_inflight_tool_does_not_grow_terminal_output_across_ticks() {
        let term_w: u16 = 80;
        let (mut r, buf) = new_capturing(term_w, 24);
        let detail = "cd /Users/theo/Documents/workspace/atomcode && cargo build 2>&1 | tail -5";

        // First render: pushes scroll-style (prev_rows=0 → fallback path).
        r.render_inflight_tool("⠋", "bash", detail, "");
        let bytes_after_first = buf.lock().unwrap().len();
        assert!(
            bytes_after_first > 0,
            "first render must emit some bytes"
        );

        // Drain so subsequent measurements are tick-only.
        buf.lock().unwrap().clear();

        // Simulate 50 spinner ticks (~4 seconds at 80ms cadence). Each
        // must take the in-place branch — no `\n`, no scroll, no
        // accumulation. We bound the total bytes by the per-tick budget
        // (~80 bytes for cursor-pos + erase + serialised row) times
        // tick count + headroom for SGR resets and wrapped continuation
        // rows. A scroll-leak would emit hundreds of bytes per tick
        // (full row content + SGR + position) and blow this bound by
        // an order of magnitude.
        for i in 0..50 {
            // Cycle through the standard braille spinner glyphs so the
            // icon arg actually changes each call. Same display width,
            // so prev_rows == new_rows and the in-place branch fires.
            let icon = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][i % 10];
            r.render_inflight_tool(icon, "bash", detail, "");
        }
        let bytes_per_tick = buf.lock().unwrap().len() / 50;
        // ~150 bytes/tick is a comfortable upper bound for the in-place
        // path (CUP + EL + serialised row + SGR resets, per wrapped row).
        // The pre-fix scroll path emitted ~600+ bytes/tick on this input
        // because each push_body_row scrolled and re-styled a fresh full
        // row at body_bottom, plus DECSTBM scroll + cursor reposition.
        assert!(
            bytes_per_tick < 300,
            "per-tick byte budget exceeded ({} bytes/tick, 50 ticks total \
             {} bytes) — render_inflight_tool is scrolling fresh rows in \
             instead of overwriting the existing ones",
            bytes_per_tick,
            buf.lock().unwrap().len()
        );

        // body_lines stays bounded too (existing invariant).
        assert!(
            r.body_lines.len() <= 4,
            "body_lines grew to {} rows across 50 ticks — should stay at \
             prev_rows count for in-place path",
            r.body_lines.len()
        );
    }

    /// User report (long `cargo install` looked stuck): the inflight
    /// tool row is `<spinner> Bash(cmd)` with no elapsed indicator,
    /// while the regular thinking spinner shows `Pondering… · 12s`.
    /// After ~30s of waiting the user can't tell whether bash is
    /// running or hung. Fix: forward the spinner-label metadata
    /// (` · 12s · N queued`) into `render_inflight_tool` so the same
    /// time anchor appears next to the tool row.
    #[test]
    fn retained_inflight_tool_renders_elapsed_meta_suffix() {
        let (mut r, _buf) = new_capturing(80, 24);
        // Seed an inflight tool so the Spinner branch routes through
        // render_inflight_tool (mirrors the real call path).
        r.render(UiLine::ToolCallInFlight {
            id: "call-1".into(),
            name: "Bash".into(),
            detail: "cargo install cargo-udeps --locked".into(),
        });
        r.render(UiLine::Spinner {
            frame: "⠋".into(),
            label: "Running Bash… · 12s".into(),
        });
        let last = r.body_lines.last().expect("inflight row expected");
        let text: String = last.iter().map(|c| c.ch).collect();
        assert!(
            text.contains("· 12s"),
            "inflight tool row missing elapsed meta suffix; got: {:?}",
            text
        );
        assert!(
            text.contains("Bash(cargo install"),
            "inflight tool row missing command detail; got: {:?}",
            text
        );
    }

    #[test]
    fn spinner_meta_suffix_extracts_after_first_separator() {
        assert_eq!(spinner_meta_suffix("Running Bash… · 12s"), " · 12s");
        assert_eq!(
            spinner_meta_suffix("Running Bash… · 12s · 2 queued"),
            " · 12s · 2 queued"
        );
        // No metadata yet (no phase clock tick) → empty suffix.
        assert_eq!(spinner_meta_suffix("Pondering…"), "");
        assert_eq!(spinner_meta_suffix(""), "");
    }

    /// Regression (screenshot 42.png): user reported a stray blinking
    /// caret at the right edge of the active `▸ Bash(...)` row, sitting
    /// alongside the legitimate input-box caret. Root cause: the
    /// in-place path in `render_inflight_tool` writes raw cursor-position
    /// bytes via `self.out.write_all` to overwrite each row, leaving the
    /// terminal cursor at end-of-row. `paint_footer` repositions the
    /// cell-model cursor to the input box but `set_cursor_visible(true)`
    /// keeps the terminal blinking — so for every 5ms paint window
    /// before the next CUP lands, the user saw two carets.
    ///
    /// Fix: hide the cursor whenever an inflight tool is active, in
    /// addition to the existing live-spinner gate. `inflight_tool.is_none()`
    /// flips back at commit time, so the cursor reappears at the input
    /// box on the next paint without a leftover blink.
    #[test]
    fn retained_inflight_tool_hides_terminal_cursor() {
        let term_w: u16 = 80;
        let (mut r, buf) = new_capturing(term_w, 24);
        let detail = "cd /Users/theo/Documents/workspace/atomcode && cargo check 2>&1 | tail -80";

        // Seed input prompt + ToolCallInFlight so paint_footer has a
        // sensible cursor position to consult.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.render(UiLine::ToolCallInFlight {
            id: "call_1".into(),
            name: "Bash".into(),
            detail: detail.into(),
        });
        // A spinner tick to exercise the in-place branch.
        r.render(UiLine::Spinner {
            frame: "⠙".into(),
            label: "Running Bash".into(),
        });
        r.flush_deferred();
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            !vterm.cursor_visible(),
            "terminal cursor must be hidden while a tool call is in flight \
             (otherwise it blinks at end-of-row alongside the input caret)"
        );

        // Commit the inflight tool — cursor must come back at the next
        // paint so the user sees their input-box caret again.
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call_1".into()),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            vterm.cursor_visible(),
            "terminal cursor must be visible again after the inflight tool \
             commits — `inflight_tool.is_none()` flips the gate back"
        );
    }

    /// Regression: user reported that after a terminal resize two
    /// footers appeared stacked on screen — old footer at pre-resize
    /// absolute rows kept its chars, new footer painted at new rows,
    /// both visible. Root cause: `Screen::resize` rebuilds both
    /// frames blank, so the next diff vs all-blank prev has nothing
    /// to erase — but the terminal still holds pre-resize glyphs at
    /// the old absolute positions.
    ///
    /// Fix: `on_resize` emits per-row CUP+EL for every row of the new
    /// viewport before repainting, so the terminal's own display
    /// clears and the new frame owns every visible column. (Uses EL
    /// instead of `\x1b[2J` because iTerm2 3.5+ has been observed to
    /// ignore ED under certain states — see `reset()` rationale.)
    #[test]
    fn retained_resize_clears_old_footer_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Frame 1: paint initial footer at 80x24 with distinctive
        // string "originaltag". After drain, the sink is empty.
        r.render(UiLine::InputPrompt {
            buf: "originaltag".into(),
            cursor_byte: 11,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(vterm.row_text(21).contains("originaltag"));

        // Resize + then push a frame with EMPTY input so the new
        // layout has no legitimate reason to contain "originaltag".
        // Any occurrence post-resize is ghost content from before.
        r.on_resize(60, 16);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // New vterm matching post-resize dimensions, feed only the
        // bytes emitted AFTER the resize (drain was called above
        // at line "assert row_text 21").
        let mut vterm = crate::test_term::VirtualTerminal::new(60, 16);
        drain_into_vterm(&buf, &mut vterm);

        for r_idx in 0..16 {
            let row = vterm.row_text(r_idx);
            assert!(
                !row.contains("originaltag"),
                "stale pre-resize content leaked to row {}: {:?}\n\
                 dump:\n{}",
                r_idx,
                row,
                vterm.dump()
            );
        }
    }

    /// Phase 7 exemplar: end-to-end render through VirtualTerminal.
    /// Verifies the same bot_rule invariant as the sibling test
    /// below — but asserts on the grid the terminal would actually
    /// paint (derived from our ANSI byte stream), not on the cell
    /// buffer we emitted from. This is the shape of test that
    /// catches "cells right, screen wrong" bugs like the Mac
    /// Terminal byte-drop issue.
    #[test]
    fn retained_bot_rule_full_width_after_wrap_via_vterm() {
        let (mut r, buf) = new_capturing(40, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(40, 24);
        let status = status_basic();

        // Frame 1: short input → 1-row middle.
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Frame 2: long input → 2-row middle. Footer grows from 5
        // to 6, bot_rule moves from row H-2 to row H-2 (same), but
        // top_rule's emit path passes through rows that previously
        // held body content.
        let long: String = std::iter::repeat('中').take(40).collect();
        r.render(UiLine::InputPrompt {
            buf: long.clone(),
            cursor_byte: long.len(),
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // bot_rule is always at absolute row H-2 = 22 (0-indexed).
        // Input box is now flush-left/right (no PAD_COL) — every col
        // 0..w should be '─' on the screen.
        let bot_rule_row = 22;
        for col in 0..40usize {
            let cell = vterm.cell_at(bot_rule_row, col);
            assert_eq!(
                cell.ch,
                '─',
                "bot_rule col {} (expected '─') shows {:?}\n\
                 full grid dump:\n{}",
                col,
                cell,
                vterm.dump()
            );
        }
    }

    /// Wide CJK input via vterm: render "你是谁" from empty, then
    /// walk the grid and confirm all three wide glyphs landed on
    /// their expected absolute columns. This is the bug class
    /// where the cell model and the byte stream disagree — here
    /// we assert the terminal's view (post-parse grid) is right.
    #[test]
    fn retained_wide_char_lands_on_screen_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        // Start with empty input (frame baseline).
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Type "你是谁" in one shot.
        r.render(UiLine::InputPrompt {
            buf: "你是谁".into(),
            cursor_byte: 9,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Screen h=24, footer 5 rows = [19, 23]:
        //   row 19: spinner blank, row 20: top rule,
        //   row 21: middle, row 22: bot rule, row 23: status.
        // "你是谁" in middle row (col 0-indexed, flush-left now):
        //   col 0 '❯', col 1 ' ',
        //   col 2 '你' (cols 2-3, right half blank), col 4 '是',
        //   col 6 '谁'.
        //   (caps_with_color has unicode_symbols=true so prompt_chevron() is "❯ ".)
        let middle_row = 21;
        assert_eq!(vterm.cell_at(middle_row, 0).ch, '\u{276f}');
        assert_eq!(vterm.cell_at(middle_row, 1).ch, ' ');
        assert_eq!(
            vterm.cell_at(middle_row, 2).ch,
            '你',
            "dump:\n{}",
            vterm.dump()
        );
        assert_eq!(vterm.cell_at(middle_row, 4).ch, '是');
        assert_eq!(vterm.cell_at(middle_row, 6).ch, '谁');
    }

    /// Menu open via vterm: the slash-command palette (4 rows)
    /// must appear on its own rows with the selected item visibly
    /// distinct (reverse video). This catches "menu item didn't
    /// paint" / "selected highlight is on wrong row" bugs on the
    /// actual screen, not just in our cell buffer.
    #[test]
    fn retained_menu_open_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        let items: Vec<(String, String)> = vec![
            ("model".into(), "Switch model".into()),
            ("provider".into(), "Add provider".into()),
            ("session".into(), "New session".into()),
            ("resume".into(), "Resume session".into()),
        ];

        // Baseline: no menu.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Open menu with selection on row 0 ('/model').
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items: items.clone(),
                selected: 0,
                    kind: crate::render::MenuKind::SlashCommand,
            }),
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Footer with menu = 1 spinner + 2 rules + 1 middle + 4 menu
        // + 1 status = 9 rows. Layout from screen_h=24:
        //   row 15: spinner blank
        //   row 16: top rule
        //   row 17: middle ("  > /")
        //   row 18: bot rule
        //   rows 19-22: menu rows (selected @ 19)
        //   row 23: status
        //
        // Inspect menu row 0 (row 19): reverse-video strip starting
        // from PAD_COL, with "▸" marker present.
        let menu0_row = 19;
        let row_text = vterm.row_text(menu0_row);
        assert!(
            row_text.contains("▸"),
            "selected marker missing on menu row 0: {:?}\ndump:\n{}",
            row_text,
            vterm.dump()
        );
        assert!(
            row_text.contains("/model"),
            "menu entry text missing: {:?}",
            row_text
        );
        // The marker cell itself should carry reverse-video.
        let arrow_col = row_text.find('▸').unwrap();
        let cell = vterm.cell_at(menu0_row, arrow_col);
        assert!(
            cell.reverse,
            "selected menu row should be reverse-video at col {} (cell={:?})",
            arrow_col, cell
        );

        // Non-selected row (menu row 1 = screen row 20) must NOT be
        // reverse-video.
        let row1_text = vterm.row_text(20);
        assert!(
            row1_text.contains("/provider"),
            "menu row 1 missing: {:?}",
            row1_text
        );
        let provider_col = row1_text.find('/').unwrap();
        assert!(
            !vterm.cell_at(20, provider_col).reverse,
            "non-selected menu row should not be reverse-video"
        );
    }

    /// Welcome via vterm: after receiving UiLine::Welcome, the
    /// six welcome lines (brand / cwd / model / blank / type hint
    /// / provider hint) must all appear on the screen above the
    /// footer.
    #[test]
    fn retained_welcome_lines_render_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/p/a".into(),
        });
        // Empty input prompt so the footer has something to paint.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Body bottom-anchored: 7 welcome rows (title + cwd + model
        // + blank + 3 hint rows) + footer 5 rows on a 24-row screen →
        // body occupies rows 12-18, footer 19-23. Verify each
        // expected piece exists somewhere in the body region.
        let found_brand = (12..=18).any(|r| vterm.row_text(r).contains("AtomCode"));
        let found_cwd = (12..=18).any(|r| vterm.row_text(r).contains("~/p/a"));
        let found_model = (12..=18).any(|r| vterm.row_text(r).contains("glm-5"));
        let found_hint = (12..=18).any(|r| vterm.row_text(r).contains("browse commands"));
        assert!(
            found_brand && found_cwd && found_model && found_hint,
            "welcome rows missing (brand={} cwd={} model={} hint={})\ndump:\n{}",
            found_brand,
            found_cwd,
            found_model,
            found_hint,
            vterm.dump()
        );
    }

    /// Regression for user report: "Mac resize 后欢迎页的内容丢了".
    /// Before this fix, on_resize cleared body_lines so the welcome
    /// transcript disappeared. Now body is preserved — resizing
    /// smaller may clip content on the right (draw_row truncates
    /// at screen.width), but "AtomCode" / cwd / model lines still
    /// read. User keeps their chat history across resize.
    ///
    /// Same issue applies on Windows identically (same code path),
    /// so the fix covers both platforms.
    #[test]
    fn retained_resize_preserves_welcome_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/p/a".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Sanity: welcome is visible pre-resize (above footer).
        let pre_has = (0..24).any(|r| vterm.row_text(r).contains("AtomCode"));
        assert!(
            pre_has,
            "welcome missing before resize\ndump:\n{}",
            vterm.dump()
        );

        // Resize smaller — welcome must still be on the new grid.
        r.on_resize(50, 16);
        r.flush_deferred();
        let mut vterm = crate::test_term::VirtualTerminal::new(50, 16);
        drain_into_vterm(&buf, &mut vterm);

        let post_has = (0..16).any(|r| vterm.row_text(r).contains("AtomCode"));
        assert!(
            post_has,
            "welcome disappeared after resize (regression of pre-fix behaviour)\n\
             dump:\n{}",
            vterm.dump()
        );
    }

    #[test]
    fn retained_resize_reflows_welcome_brand_row_when_expanding() {
        let (mut r, buf) = new_capturing(40, 18);

        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/p/a".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let mut pre = crate::test_term::VirtualTerminal::new(40, 18);
        drain_into_vterm(&buf, &mut pre);

        r.on_resize(80, 18);
        r.flush_deferred();
        let mut post = crate::test_term::VirtualTerminal::new(80, 18);
        drain_into_vterm(&buf, &mut post);

        let brand_row = (0..18)
            .map(|row| post.row_text(row))
            .find(|row| row.contains("AtomCode"))
            .expect("brand row should remain visible after widening");
        let atom_idx = brand_row.find("AtomCode").unwrap();
        let ver_idx = brand_row
            .find(concat!("v", env!("CARGO_PKG_VERSION")))
            .unwrap();
        let lic_idx = brand_row.find("MIT").unwrap();

        assert!(
            ver_idx > atom_idx + 20,
            "version should move right after widening, row={:?}",
            brand_row
        );
        assert!(
            lic_idx > ver_idx,
            "license should stay on the same row after widening, row={:?}",
            brand_row
        );
    }

    #[test]
    fn retained_resize_reflows_welcome_brand_row_when_shrinking() {
        let (mut r, buf) = new_capturing(80, 18);

        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/p/a".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let mut pre = crate::test_term::VirtualTerminal::new(80, 18);
        drain_into_vterm(&buf, &mut pre);

        r.on_resize(24, 18);
        r.flush_deferred();
        let mut post = crate::test_term::VirtualTerminal::new(24, 18);
        drain_into_vterm(&buf, &mut post);

        let brand_row = (0..18)
            .map(|row| post.row_text(row))
            .find(|row| row.contains("AtomCode"))
            .expect("brand row should remain visible after shrinking");
        let version_row = (0..18)
            .map(|row| post.row_text(row))
            .find(|row| row.contains(concat!("v", env!("CARGO_PKG_VERSION"))))
            .expect("version row should remain visible after shrinking");
        assert!(
            version_row.contains(concat!("v", env!("CARGO_PKG_VERSION"))),
            "version should remain visible after shrinking, brand_row={:?}, version_row={:?}",
            brand_row,
            version_row
        );
        assert!(
            version_row.contains("MIT"),
            "license should remain visible after shrinking, brand_row={:?}, version_row={:?}",
            brand_row,
            version_row
        );
    }

    /// Regression: after a resize-smaller drag, cached `body_lines` rows
    /// built against the OLD terminal width were re-emitted verbatim. Rows
    /// wider than the new width triggered the real terminal's auto-wrap;
    /// the wrapped tail spilled into footer / scroll-region rows, producing
    /// the visible "everything shifted and the footer has garbage in it"
    /// glitch users reported after dragging the window narrower.
    ///
    /// `VirtualTerminal::put_char` silently drops cells past the grid's
    /// right edge (no auto-wrap modelled), so we can't observe the bug
    /// at the grid level. Assert on the emitted byte stream instead:
    /// between any two cursor-positioning CSIs, the printable payload
    /// must fit within the new `screen.width()`.
    #[test]
    fn retained_resize_clips_wide_body_rows_to_new_width() {
        let (mut r, buf) = new_capturing(120, 24);

        // Seed body with a long tool call: a `▸ Name(payload)` row whose
        // display width far exceeds any sane "shrink-to" target.
        r.render(UiLine::ToolCall {
            name: "Bash".into(),
            detail: "X".repeat(100),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        // Discard pre-resize bytes — this test only asserts on what
        // `on_resize` emits at the narrower width.
        buf.lock().unwrap().clear();

        let new_w: u16 = 40;
        r.on_resize(new_w, 16);

        // Parse the emitted stream: CSI sequences delimit "runs" of
        // printable bytes. Every run must fit within the new width.
        // `\n` also delimits (emit_body_line_inner uses raw LF to scroll
        // the DECSTBM region).
        let bytes = buf.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&bytes);
        let mut runs: Vec<String> = vec![String::new()];
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // CSI / ESC dispatch — eat until the final byte. The
                // final byte delimits the current run from the next.
                runs.push(String::new());
                if chars.peek() == Some(&'[') {
                    chars.next();
                    while let Some(&p) = chars.peek() {
                        chars.next();
                        if p.is_ascii_alphabetic() || p == '~' {
                            break;
                        }
                    }
                } else if chars.peek() == Some(&']') {
                    // OSC — eat until ST (BEL or ESC\)
                    while let Some(&p) = chars.peek() {
                        chars.next();
                        if p == '\x07' {
                            break;
                        }
                    }
                }
                continue;
            }
            if c == '\n' || c == '\r' {
                runs.push(String::new());
                continue;
            }
            runs.last_mut().unwrap().push(c);
        }

        for run in &runs {
            let w = crate::width::display_width(run);
            assert!(
                w <= new_w as usize,
                "body re-emit produced a {}-col run on a {}-col terminal: {:?}\n\
                 (clip_cells_to_width should have trimmed this before emit)",
                w,
                new_w,
                run,
            );
        }
    }

    #[test]
    fn retained_welcome_reflows_path_model_and_hints_on_narrow_terminal() {
        // 22-col WIDTH is the test's actual subject (column reflow).
        // Use 26-row HEIGHT — large enough that the reflowed banner
        // (title × 2 + path × 4 + model × 2 + blank + hint_a × 3 +
        // hint_b × 2 + hint_c × 3 = 17 body rows, plus 4 footer rows)
        // fits entirely in the viewport with headroom. With a 20-row
        // viewport the brand line scrolled into scrollback and made the
        // assertion brittle to small additions to the hint block.
        let (mut r, buf) = new_capturing(22, 26);
        let mut vterm = crate::test_term::VirtualTerminal::new(22, 26);

        r.render(UiLine::Welcome {
            model: "MiniMax-M2.7-long".into(),
            working_dir: "~/workspace/gitcode_project/atomcode_family/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        assert!(
            (0..26).any(|row| vterm.row_text(row).contains("AtomCode")),
            "brand missing on narrow terminal\n{}",
            vterm.dump()
        );
        assert!(
            (0..26).any(|row| vterm.row_text(row).contains("workspace")),
            "path should wrap instead of disappearing on narrow terminal\n{}",
            vterm.dump()
        );
        assert!(
            (0..26).any(|row| vterm.row_text(row).contains("MiniMax")),
            "model should wrap instead of disappearing on narrow terminal\n{}",
            vterm.dump()
        );
        assert!(
            (0..26).any(|row| vterm.row_text(row).contains("type something")),
            "welcome input hint should remain visible on narrow terminal\n{}",
            vterm.dump()
        );
        assert!(
            (0..26).any(|row| vterm.row_text(row).contains("commands")),
            "welcome commands hint should remain visible on narrow terminal\n{}",
            vterm.dump()
        );
        assert!(
            (0..26).any(|row| vterm.row_text(row).contains("/provider")),
            "provider hint should remain visible on narrow terminal\n{}",
            vterm.dump()
        );
    }

    /// User echo: `UiLine::User("hi")` produces a body row with
    /// `> hi` accent prefix + a blank spacer. Grid-verified at
    /// absolute rows right above the footer (body bottom-anchored).
    #[test]
    fn retained_user_echo_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::User("你好 world".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        // User line + blank spacer = 2 body rows somewhere in the
        // body area (scrollback-push layout is stack-like, exact
        // row depends on how many rows have been pushed).
        // Prompt glyph depends on caps.unicode_symbols; caps_with_color
        // is UTF-8 + non-dumb so `prompt_chevron()` returns `❯ `.
        let found = vterm.any_row(|row| {
            row.contains('\u{276f}')
                && row.contains('你')
                && row.contains('好')
                && row.contains("world")
        });
        assert!(found, "user echo missing\ndump:\n{}", vterm.dump());
    }

    /// User-echo chevron must sit at col 0 — the same column as the
    /// input-box chevron below — so history symbols align with the
    /// live prompt.
    #[test]
    fn retained_user_echo_chevron_at_col_0() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::User("hello".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let row_idx = (0..vterm.height() as usize)
            .find(|&i| {
                vterm.row_text(i).contains('\u{276f}') && vterm.row_text(i).contains("hello")
            })
            .unwrap_or_else(|| panic!("user echo row missing\ndump:\n{}", vterm.dump()));
        assert_eq!(
            vterm.cell_at(row_idx, 0).ch,
            '\u{276f}',
            "user-echo chevron must land at col 0, got row: {:?}\ndump:\n{}",
            vterm.row_text(row_idx),
            vterm.dump()
        );
    }

    /// ToolCall: `● name(detail)` formatted. Grid-verifies the
    /// marker + name + parens appear together on one row.
    #[test]
    fn retained_tool_call_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::ToolCall {
            name: "bash".into(),
            detail: "ls -la".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let found = vterm
            .any_row(|row| row.contains("●") && row.contains("bash") && row.contains("ls -la"));
        assert!(found, "tool call missing\ndump:\n{}", vterm.dump());
    }

    /// ToolCall glyph `●` must sit at col 0, same baseline as user
    /// echo and input chevron.
    #[test]
    fn retained_tool_call_arrow_at_col_0() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::ToolCall {
            name: "bash".into(),
            detail: "ls -la".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let row_idx = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("●") && vterm.row_text(i).contains("bash"))
            .unwrap_or_else(|| panic!("tool call row missing\ndump:\n{}", vterm.dump()));
        assert_eq!(
            vterm.cell_at(row_idx, 0).ch,
            '●',
            "tool-call glyph must land at col 0, got row: {:?}\ndump:\n{}",
            vterm.row_text(row_idx),
            vterm.dump()
        );
    }

    /// ToolResult success: `⎿ summary` + blank spacer; failure
    /// prepends `✗ `. We test success path here; the error styling
    /// (Role::Error red) is a cell-style detail not asserted in
    /// this grid check.
    #[test]
    fn retained_tool_result_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::ToolResult {
            success: true,
            summary: "3 files changed".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let found = vterm.any_row(|row| row.contains("└") && row.contains("3 files changed"));
        assert!(found, "tool result missing\ndump:\n{}", vterm.dump());
    }

    /// ToolResult `⎿` glyph sits at col 2 — directly under the tool
    /// name's leading character (a `▸ Bash(...)` row puts `▸` at col 0
    /// and `B` at col 2, so the result body's `⎿` aligns vertically
    /// with the `B`). Matches Claude Code's tool-result layout
    /// (screenshot 46) and reads tighter than the previous 4-space
    /// indent which left `⎿` floating two columns past the tool name.
    #[test]
    fn retained_tool_result_arrow_at_col_2() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::ToolResult {
            success: true,
            summary: "3 files changed".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let row_idx = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("└") && vterm.row_text(i).contains("3 files"))
            .unwrap_or_else(|| panic!("tool result row missing\ndump:\n{}", vterm.dump()));
        assert_eq!(
            vterm.cell_at(row_idx, 2).ch,
            '└',
            "tool-result glyph must land at col 2, got row: {:?}\ndump:\n{}",
            vterm.row_text(row_idx),
            vterm.dump()
        );
        for c in 0..2 {
            assert_eq!(
                vterm.cell_at(row_idx, c).ch,
                ' ',
                "cols 0..2 before ⎿ must be blank, col {} is {:?}",
                c,
                vterm.cell_at(row_idx, c).ch,
            );
        }
    }

    /// End-to-end alignment pin: the `⎿` glyph of a `ToolResult` must
    /// land in the same column as the first character of the tool
    /// name in the `▸ Tool(...)` row directly above it. Catches future
    /// drift in either the tool-call prefix (`"▸ "`) or the result
    /// prefix (`"  ⎿ "`) — they have to stay coupled or the visual
    /// "tool name ↔ ⎿ (its result)" anchor breaks.
    ///
    /// Iterates over a representative cross-section of tool types
    /// (Bash, Grep, Glob, ReadFile, EditFile) — the result-row prefix
    /// is dispatched from a single generic `UiLine::ToolResult` arm,
    /// not branched on tool name, so any drift would surface here for
    /// every tool simultaneously. Test names that are NOT verified
    /// here (e.g. WriteFile, SearchReplace, TraceCallers) all share
    /// the same code path — covering the cross-section is enough to
    /// prove universality.
    #[test]
    fn retained_tool_result_arrow_aligns_for_every_tool_type() {
        // Each entry: tool name + a sample summary. The first
        // character of `name` is the alignment anchor on the tool-call
        // row; the `⎿` on the result row must sit in the same column.
        let cases: &[(&str, &str)] = &[
            ("Bash", "[elapsed: 0.0s, exit: 0] (1 line)"),
            ("Grep", "203 matches in 18 files"),
            ("Glob", "12 files found:"),
            ("ReadFile", "1| use anyhow::Result;"),
            ("EditFile", "Edited /tmp/foo.rs (3 lines changed)"),
        ];

        for (tool_name, summary) in cases {
            let (mut r, buf) = new_capturing(120, 24);
            let mut vterm = crate::test_term::VirtualTerminal::new(120, 24);
            let status = status_basic();
            r.render(UiLine::ToolCall {
                name: (*tool_name).into(),
                detail: "args".into(),
            });
            r.render(UiLine::ToolResult {
                success: true,
                summary: (*summary).into(),
            });
            r.render(UiLine::InputPrompt {
                buf: String::new(),
                cursor_byte: 0,
                menu: None,
                status: status.clone(),
                attachments: Vec::new(),
            });
            r.flush_deferred();
            drain_into_vterm(&buf, &mut vterm);

            let tool_row = (0..vterm.height() as usize)
                .find(|&i| {
                    vterm.row_text(i).contains("●") && vterm.row_text(i).contains(tool_name)
                })
                .unwrap_or_else(|| {
                    panic!("[{tool_name}] tool call row missing\ndump:\n{}", vterm.dump())
                });
            let result_row = (0..vterm.height() as usize)
                .find(|&i| vterm.row_text(i).contains("└"))
                .unwrap_or_else(|| {
                    panic!("[{tool_name}] tool result row missing\ndump:\n{}", vterm.dump())
                });

            let first_char = tool_name.chars().next().unwrap();
            let name_col = (0..vterm.width() as usize)
                .find(|&c| vterm.cell_at(tool_row, c).ch == first_char)
                .unwrap_or_else(|| {
                    panic!(
                        "[{tool_name}] first char {first_char:?} not found on tool row: {:?}",
                        vterm.row_text(tool_row)
                    )
                });
            let arrow_col = (0..vterm.width() as usize)
                .find(|&c| vterm.cell_at(result_row, c).ch == '└')
                .unwrap_or_else(|| {
                    panic!(
                        "[{tool_name}] '└' not found on result row: {:?}",
                        vterm.row_text(result_row)
                    )
                });
            assert_eq!(
                arrow_col, name_col,
                "[{tool_name}] result '└' col {} must match tool name {:?} col {} \
                 (tool row: {:?}, result row: {:?})",
                arrow_col,
                first_char,
                name_col,
                vterm.row_text(tool_row),
                vterm.row_text(result_row),
            );
        }
    }

    /// Failure ToolResult: header line is bold red (so users still get
    /// the "this is bad" signal) but continuation lines fall back to
    /// default fg (so quoted code in error messages — common with
    /// edit_file's "old_string not found" path — doesn't blend visually
    /// with diff-remove blocks. See retained.rs UiLine::ToolResult arm.
    #[test]
    fn retained_tool_result_failure_header_red_body_default() {
        let (mut r, buf) = new_capturing(120, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(120, 24);
        let status = status_basic();
        // Multi-line failure body: header + quoted-code detail.
        r.render(UiLine::ToolResult {
            success: false,
            summary: "old_string not found in foo.rs\n759| line content\n760| more code".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Header row: contains the ✗ glyph, cells must be bold + red.
        let header_idx = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("✗") && vterm.row_text(i).contains("not found"))
            .unwrap_or_else(|| panic!("header row missing\ndump:\n{}", vterm.dump()));
        let header_text = vterm.row_text(header_idx);
        let glyph_col = header_text.find('✗').unwrap();
        let header_cell = vterm.cell_at(header_idx, glyph_col);
        assert_eq!(
            header_cell.fg,
            Some(crossterm::style::Color::Red),
            "header `✗` must be red, got {:?}",
            header_cell,
        );
        assert!(
            header_cell.bold,
            "header `✗` must be bold, got {:?}",
            header_cell,
        );

        // Continuation row: contains the quoted code "759|"; must NOT
        // be red (so it stops looking like a diff-remove block).
        let cont_idx = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("759|"))
            .unwrap_or_else(|| panic!("continuation row missing\ndump:\n{}", vterm.dump()));
        let cont_text = vterm.row_text(cont_idx);
        let digit_col = cont_text.find("759|").unwrap();
        let cont_cell = vterm.cell_at(cont_idx, digit_col);
        assert_ne!(
            cont_cell.fg,
            Some(crossterm::style::Color::Red),
            "continuation row must NOT be red (would alias visually with diff-remove): {:?}",
            cont_cell,
        );
    }

    /// `└` is a leaf marker for the whole tool-result block, not a
    /// per-line bullet. When the body wraps to multiple visual rows
    /// (narrow terminal, long summary, or `\n`-separated lines) only
    /// the FIRST visual row carries `└`; continuation rows align under
    /// the text via 4 spaces. Without this, every wrapped chunk shows
    /// a redundant `└` at col 2 — the bug fixed alongside this test.
    #[test]
    fn retained_tool_result_wrap_continuation_has_no_arrow() {
        // 40-col width → row_w = 40 - PAD_COL(2) - prefix(4) = 34.
        // Summary is > 34 cols so it must wrap to at least 2 visual rows.
        let (mut r, buf) = new_capturing(40, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(40, 24);
        let status = status_basic();
        let long_summary =
            "Created new file /tmp/atomcode-smoke-temp-check.txt (15 bytes, 1 line)";
        r.render(UiLine::ToolResult {
            success: true,
            summary: long_summary.into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let arrow_rows: Vec<usize> = (0..vterm.height() as usize)
            .filter(|&i| vterm.row_text(i).contains('└'))
            .collect();
        assert_eq!(
            arrow_rows.len(),
            1,
            "`└` must appear on exactly one row (the first), found {} rows. dump:\n{}",
            arrow_rows.len(),
            vterm.dump()
        );
        let first_row = arrow_rows[0];
        // The text just after `└ ` should appear on the first row.
        assert!(
            vterm.row_text(first_row).contains("Created new file"),
            "first row must carry the head of the body, got: {:?}",
            vterm.row_text(first_row)
        );

        // A continuation row exists (the body wrapped) and it must
        // start with 4 spaces (cols 0..4) — same width as `"  └ "` —
        // so the text aligns under the head text, not under the `└`.
        let cont_row = first_row + 1;
        assert!(
            (cont_row as u16) < vterm.height(),
            "expected at least one continuation row, vterm height = {}",
            vterm.height()
        );
        for c in 0..4 {
            assert_eq!(
                vterm.cell_at(cont_row, c).ch,
                ' ',
                "continuation row col {} must be blank, got {:?} (row text: {:?})",
                c,
                vterm.cell_at(cont_row, c).ch,
                vterm.row_text(cont_row),
            );
        }
    }

    /// DiffBlock: multiple added/removed lines, each with its own
    /// marker. Grid-verifies `+` and `-` both appear in the
    /// respective rows at the correct indent (7-space prefix).
    #[test]
    fn retained_diff_block_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::DiffBlock(vec![
            super::super::DiffEntry {
                added: true,
                text: "new line".into(),
            },
            super::super::DiffEntry {
                added: false,
                text: "old line".into(),
            },
        ]));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let has_added = vterm.any_row(|r| r.contains("+") && r.contains("new line"));
        let has_removed = vterm.any_row(|r| r.contains("-") && r.contains("old line"));
        assert!(has_added, "added row missing\ndump:\n{}", vterm.dump());
        assert!(has_removed, "removed row missing\ndump:\n{}", vterm.dump());
    }

    /// TurnSeparator: blank + `──── Label ────` + blank. The rule
    /// spans the full content width with the label centred.
    #[test]
    fn retained_turn_separator_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::TurnSeparator {
            label: "Sealed · 1 turn".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let found = vterm
            .any_row(|row| row.contains("─") && row.contains("Sealed") && row.contains("1 turn"));
        assert!(found, "separator missing\ndump:\n{}", vterm.dump());
    }

    /// TurnSeparator rule must render dim (default fg + SGR 2) — not
    /// pinned to a bright muted color. v4.23.0 broadened MUTED_DARK to
    /// SGR 37 (light gray) so child rows of tool batches read on Warp
    /// dark, but reusing `Role::Muted` here made the `resumed:` rule
    /// blend into body text. The fix: this rule is decoration, so it
    /// should use the same SGR-2 dim that alt_screen uses, leaving fg
    /// at terminal default.
    ///
    /// Two complementary assertions are needed: the vterm grid only
    /// tracks fg/bold/reverse (no faint/dim field), so `cell.fg.is_none()`
    /// alone wouldn't catch a regression to `style_for(Role::Secondary)`
    /// — that also has `fg=None` but drops the `\x1b[2m`, leaving the
    /// rule at full intensity. We pin the byte stream too so the dim
    /// requirement survives a future refactor.
    #[test]
    fn retained_turn_separator_rule_uses_default_fg() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::TurnSeparator {
            label: "resumed: mcp with plan mode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Snapshot raw bytes BEFORE `drain_into_vterm` consumes the
        // buffer — we need to inspect the SGR stream that the vterm
        // grid can't represent.
        let raw_bytes = buf.lock().unwrap().clone();
        drain_into_vterm(&buf, &mut vterm);

        let row_idx = (0..vterm.height() as usize)
            .find(|&r| vterm.row_text(r).contains("─") && vterm.row_text(r).contains("resumed"))
            .unwrap_or_else(|| panic!("separator row missing\ndump:\n{}", vterm.dump()));
        let row_text = vterm.row_text(row_idx);
        let rule_col = row_text.find('─').unwrap();
        let rule_cell = vterm.cell_at(row_idx, rule_col);
        assert!(
            rule_cell.fg.is_none(),
            "separator rule should use terminal-default fg (dimmed via SGR 2), \
             not a pinned color — got fg={:?}",
            rule_cell.fg,
        );

        // Same contract on the `resumed:` label cell — rule and label
        // share one `CellStyle`; a future split that recolours only the
        // label would silently break the "quiet decoration" intent.
        let label_col = row_text.find('r').expect("`resumed` label missing");
        let label_cell = vterm.cell_at(row_idx, label_col);
        assert!(
            label_cell.fg.is_none(),
            "`resumed:` label should share the rule's default-fg style — got fg={:?}",
            label_cell.fg,
        );

        // Byte-stream guard: `\x1b[2m` MUST appear in the rendered
        // output. Catches a regression to `style_for(Role::Secondary)`
        // — same `fg=None` so vterm assertions above pass, but no dim
        // is emitted and the rule visually matches body text again.
        let bytes_str = String::from_utf8_lossy(&raw_bytes);
        assert!(
            bytes_str.contains("\x1b[2m"),
            "separator must emit SGR 2 (faint) so terminal renders the rule \
             and label dimmed against body text; got bytes (truncated): {:?}",
            &bytes_str[..bytes_str.len().min(400)],
        );
    }

    /// Error line: `[Error: msg]` body row with red fg — we assert
    /// the text + the fg style on the '[' cell.
    #[test]
    fn retained_error_line_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::Error("connection lost".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        // Find the row containing the error payload (layout-agnostic).
        let row_idx = (0..vterm.height() as usize)
            .find(|&r| {
                let t = vterm.row_text(r);
                t.contains("[Error:") && t.contains("connection lost")
            })
            .unwrap_or_else(|| panic!("error message missing\ndump:\n{}", vterm.dump()));
        let row_text = vterm.row_text(row_idx);
        let idx = row_text.find('[').unwrap();
        let cell = vterm.cell_at(row_idx, idx);
        assert!(
            cell.fg.is_some(),
            "error text should have a foreground color"
        );
    }

    /// Regression (screenshot 47.png): adjacent bash blocks with NO
    /// blank line between them — the previous fix (screenshot 44)
    /// over-corrected by stripping the trailing `\n` from the Ctrl+O
    /// hint, removing the breathing-row separator. The `\n` IS
    /// load-bearing: callers append it to mean "give me one blank row
    /// after this for visual separation." Internal `\n`s split into
    /// multiple rows; a trailing `\n` adds a single blank tail row.
    #[test]
    fn retained_command_output_trailing_newline_pushes_blank_separator() {
        let (mut r, _buf) = new_capturing(80, 24);
        let before = r.body_lines.len();
        r.render(UiLine::CommandOutput(
            "  ○ Press Ctrl+O to show real-time output\n".into(),
        ));
        let pushed = r.body_lines.len() - before;
        assert_eq!(
            pushed, 2,
            "trailing \\n must push 1 content row + 1 blank separator — \
             expected 2 rows, got {}. Adjacent bash blocks rely on this \
             blank to visually break apart in scrollback.",
            pushed
        );

        // Confirm the second row is actually blank (whitespace only),
        // so future drift in `wrap_line_to_width` for `""` would still
        // be caught here.
        let last = r.body_lines.last().unwrap();
        assert!(
            last.iter().all(|c| c.ch == ' '),
            "second row must be whitespace-only, got: {:?}",
            last.iter().map(|c| c.ch).collect::<String>()
        );
    }

    /// Internal `\n`s split into rows (existing invariant — separate
    /// from the trailing-`\n` behavior above): `"a\nb\nc"` is three
    /// content rows, `"a\nb\nc\n"` is three content rows + one blank
    /// tail row.
    #[test]
    fn retained_command_output_internal_newlines_split_into_rows() {
        let (mut r, _buf) = new_capturing(80, 24);
        let before = r.body_lines.len();
        r.render(UiLine::CommandOutput("line one\nline two\nline three".into()));
        let pushed = r.body_lines.len() - before;
        assert_eq!(
            pushed, 3,
            "three internal lines, no trailing \\n → 3 rows, got {}",
            pushed
        );

        // Trailing `\n` adds one blank to the existing three lines.
        let before = r.body_lines.len();
        r.render(UiLine::CommandOutput("a\nb\nc\n".into()));
        let pushed = r.body_lines.len() - before;
        assert_eq!(
            pushed, 4,
            "three internal lines + trailing \\n → 4 rows (3 content + 1 blank), got {}",
            pushed
        );
    }

    /// CommandOutput: `/command` return string rendered as body.
    /// Used by /model, /login, /provider etc. to echo status lines.
    #[test]
    fn retained_command_output_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::CommandOutput(
            "Switched to glm5 · Pro/zai-org/GLM-5".into(),
        ));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let found = vterm.any_row(|row| row.contains("Switched to glm5"));
        assert!(found, "command output missing\ndump:\n{}", vterm.dump());
    }

    /// After moving ▶ to col 0, `pop_approval_prompt` must still
    /// detect the approval rows via col 0 and must NOT be fooled by
    /// an adjacent ● tool-call row (also at col 0, different glyph).
    /// In an 80-col terminal the label + chips fit on one line, so
    /// pop_approval_prompt removes a single row.
    #[test]
    fn retained_approval_pop_still_detects_glyph() {
        let (mut r, _buf) = new_capturing(80, 24);

        r.render(UiLine::ToolCall {
            name: "bash".into(),
            detail: "ls".into(),
        });
        r.render(UiLine::ApprovalPrompt {
            tool: "bash".into(),
            detail: "ls".into(),
        });
        let before = r.body_lines.len();
        r.pop_approval_prompt();
        let after = r.body_lines.len();
        assert_eq!(
            before - after,
            1,
            "pop_approval_prompt should drop the single label+chips row"
        );

        // Second call: last row is now the tool-call `●`, not `▶`.
        // Must be a no-op.
        let before2 = r.body_lines.len();
        r.pop_approval_prompt();
        let after2 = r.body_lines.len();
        assert_eq!(
            before2, after2,
            "pop_approval_prompt must not drop non-approval rows"
        );
    }

    /// When the approval label wraps across multiple lines (narrow
    /// terminal), pop_approval_prompt must remove ALL of them: the
    /// wrapped label rows + the chips row.
    #[test]
    fn retained_approval_pop_multiline() {
        // 30-col terminal: "▶ 等待审批：Bash(a very long command)"
        // should wrap the label, producing 2+ label rows + 1 chips row.
        let (mut r, _buf) = new_capturing(30, 24);

        r.render(UiLine::ToolCall {
            name: "bash".into(),
            detail: "a very long command".into(),
        });
        r.render(UiLine::ApprovalPrompt {
            tool: "bash".into(),
            detail: "a very long command".into(),
        });
        let before = r.body_lines.len();
        r.pop_approval_prompt();
        let after = r.body_lines.len();
        // Should pop at least the chips row + the ▶ header row.
        // If the label wrapped, it pops even more.
        assert!(
            before - after >= 2,
            "pop_approval_prompt should drop at least 2 rows (label + chips), got {}",
            before - after
        );

        // Second call: no more approval rows — must be a no-op.
        let before2 = r.body_lines.len();
        r.pop_approval_prompt();
        let after2 = r.body_lines.len();
        assert_eq!(
            before2, after2,
            "pop_approval_prompt must not drop non-approval rows"
        );
    }

    /// Regression: when the user approves a tool (presses Y/A/N),
    /// `pop_approval_prompt` must NOT erase the footer (input box,
    /// top/bot rules, status bar) from the terminal. Earlier versions
    /// used `\x1b[J` from `body_bottom;1` which erased to end-of-screen
    /// — i.e. through the footer — and the cell-diff cache then prevented
    /// the footer from being redrawn (cells unchanged → no diff →
    /// no emit), leaving the user with no visible input prompt.
    #[test]
    fn retained_pop_approval_preserves_footer() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Paint a full frame with an active footer (status bar visible).
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        // Confirm baseline: status row visible.
        assert!(
            vterm.any_row(|row| row.contains("glm-5")),
            "baseline: status row should be on screen\ndump:\n{}",
            vterm.dump()
        );

        // Now render an approval prompt and pop it.
        r.render(UiLine::ToolCall {
            name: "bash".into(),
            detail: "ls".into(),
        });
        r.render(UiLine::ApprovalPrompt {
            tool: "bash".into(),
            detail: "ls".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        r.pop_approval_prompt();
        // Trigger a new paint cycle (mirrors what happens after the
        // user presses Y and the agent emits the next body event).
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Footer (status bar) must still be visible. Before the fix
        // this assertion failed: pop_approval_prompt's `\x1b[J`
        // erased the status row, and the diff cache stopped paint_footer
        // from re-emitting it.
        assert!(
            vterm.any_row(|row| row.contains("glm-5")),
            "input box / status row should still be on screen after \
             approval pop\ndump:\n{}",
            vterm.dump()
        );
    }

    /// StreamingBox / Spinner: the `frame + label` pair now lives in
    /// the BODY (not the footer) as an animated "live" row at
    /// body_bottom. The emoji/frame is flush-left at col 0 — same
    /// gutter as `▸` tool calls and `❯` user echoes — because the
    /// previous footer position (col 2, inside PAD_COL margin) left
    /// it visually misaligned with surrounding body paragraphs.
    #[test]
    fn retained_spinner_renders_as_body_row_flush_left() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Thinking".into(),
            status: status.clone(),
            menu: None,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Spinner must appear on the LAST body row (just above the
        // footer's top_rule), with the frame at col 0.
        // Footer with 4 rows on h=24 → top_rule at row 20 (0-idx),
        // so last body row = 0-idx row 19.
        let spinner_row = vterm.row_text(19);
        assert!(
            spinner_row.contains("⠋") && spinner_row.contains("Thinking"),
            "spinner not found on last body row (got {:?}):\n{}",
            spinner_row,
            vterm.dump()
        );
        // Frame glyph at absolute col 0 — flush-left with body paragraphs.
        assert_eq!(
            vterm.cell_at(19, 0).ch,
            '⠋',
            "expected frame at col 0, found {:?}:\n{}",
            vterm.cell_at(19, 0).ch,
            vterm.dump()
        );

        // Footer no longer hosts the spinner — the row right above
        // top_rule (which USED to be the spinner slot) must be empty
        // of any spinner glyphs. With the new footer geometry
        // (4 rows: top_rule / middle / bot_rule / status on h=24),
        // row 20 is top_rule and the ex-spinner slot no longer exists.
        let top_rule_row = vterm.row_text(20);
        assert!(
            !top_rule_row.contains("Thinking"),
            "footer row still carries spinner label: {:?}:\n{}",
            top_rule_row,
            vterm.dump()
        );
    }

    /// Consecutive Spinner ticks must UPDATE the same body row
    /// in-place (animation), not push a new row each tick — otherwise
    /// 100ms of animation at 80ms/frame would accumulate 1 row per
    /// frame and scroll the user's actual history off-screen in
    /// seconds.
    #[test]
    fn retained_consecutive_spinner_ticks_update_same_body_row() {
        let (mut r, _buf) = new_capturing(80, 24);
        let status = status_basic();
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Thinking".into(),
            status: status.clone(),
            menu: None,
            attachments: Vec::new(),
        });
        let after_first = r.body_lines.len();
        assert!(
            after_first >= 1,
            "spinner event must push at least 1 body row (got {})",
            after_first
        );

        // 9 more spinner frames — the usual Braille cycle.
        for frame in ["⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] {
            r.render(UiLine::StreamingBox {
                buf: String::new(),
                cursor_byte: 0,
                frame,
                label: "Thinking".into(),
                status: status.clone(),
                menu: None,
                attachments: Vec::new(),
            });
        }
        assert_eq!(
            r.body_lines.len(),
            after_first,
            "spinner ticks grew body_lines from {} to {} — each tick \
            must update the same row, not append",
            after_first,
            r.body_lines.len()
        );
    }

    /// AssistantText arriving after a live spinner COVERS the
    /// spinner row (it's a transient indicator, not a historical
    /// paragraph header). Answer text appears exactly where
    /// `⠋ Pondering…` was, no stacked ghost, no scrollback pollution.
    #[test]
    fn retained_assistant_text_covers_spinner_row() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Pondering".into(),
            status: status.clone(),
            menu: None,
            attachments: Vec::new(),
        });
        r.render(UiLine::AssistantText("Hello world\n".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Spinner must be GONE from the visible grid — assistant
        // text has overwritten its row.
        let has_spinner = vterm.any_row(|row| row.contains("⠋") && row.contains("Pondering"));
        let has_text = vterm.any_row(|row| row.contains("Hello world"));
        assert!(
            !has_spinner,
            "spinner still visible after AssistantText — it must be \
             covered, not frozen:\n{}",
            vterm.dump()
        );
        assert!(has_text, "assistant text missing:\n{}", vterm.dump());

        // And removed from history: body_lines should not carry a
        // lingering spinner entry that would re-surface on
        // ensure_scroll_region repaints or resize.
        let spinner_in_history = r.body_lines.iter().any(|row| {
            let text: String = row.iter().map(|c| c.ch).collect();
            text.contains("Pondering")
        });
        assert!(
            !spinner_in_history,
            "spinner row still in body_lines — it must be popped when \
             covered"
        );
    }

    /// Models commonly emit a leading `\n` (or several) before
    /// actual reply text — a warm-up that prior code treated as a
    /// paragraph-boundary blank because the tail was the live
    /// spinner (non-blank cells, fails `tail_blank` check). Result
    /// was a ghost blank row between the user message spacer and
    /// the first real content. Fix: treat "tail is live spinner"
    /// the same as "tail is blank" — the spinner is transient, not
    /// a paragraph we need to visually separate from.
    #[test]
    fn retained_leading_blank_assistant_text_does_not_add_ghost_row() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        r.render(UiLine::User("hi-from-user".into()));
        r.flush_deferred();
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Pondering".into(),
            status: status.clone(),
            menu: None,
            attachments: Vec::new(),
        });
        // Leading `\n` warm-up from the model — this is the case
        // that produces the ghost blank before the fix.
        r.render(UiLine::AssistantText("\n".into()));
        // Then the real content.
        r.render(UiLine::AssistantText("Hello world\n".into()));
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let user_row = (0..24)
            .find(|r| vterm.row_text(*r).contains("hi-from-user"))
            .unwrap_or_else(|| panic!("user echo missing:\n{}", vterm.dump()));
        let hello_row = (0..24)
            .find(|r| vterm.row_text(*r).contains("Hello world"))
            .unwrap_or_else(|| panic!("Hello world missing:\n{}", vterm.dump()));

        // Exactly ONE blank between user and assistant (the
        // user-message spacer). A ghost blank would make it 2.
        assert_eq!(
            hello_row - user_row,
            2,
            "expected 1 blank row between user and assistant, got {} \
             blank row(s) — leading `\\n` from model created a ghost \
             spacer:\n{}",
            hello_row.saturating_sub(user_row).saturating_sub(1),
            vterm.dump()
        );
    }

    /// Realistic flow: user sends a message → spinner shows →
    /// assistant text streams in. The assistant text must land on
    /// EXACTLY the spinner's row (no empty row between spinner's
    /// former slot and the new text). User-message blank spacer is
    /// still there (it lives above the spinner's slot), but no
    /// additional blank gets introduced by clear_live_spinner.
    #[test]
    fn retained_spinner_replacement_leaves_no_extra_blank() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        r.render(UiLine::User("hi-from-user".into()));
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Pondering".into(),
            status: status.clone(),
            menu: None,
            attachments: Vec::new(),
        });
        r.render(UiLine::AssistantText("Hello world\n".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Find the rows that carry our 3 markers.
        let user_row = (0..24)
            .find(|r| vterm.row_text(*r).contains("hi-from-user"))
            .unwrap_or_else(|| panic!("user echo row missing:\n{}", vterm.dump()));
        let hello_row = (0..24)
            .find(|r| vterm.row_text(*r).contains("Hello world"))
            .unwrap_or_else(|| panic!("assistant text row missing:\n{}", vterm.dump()));

        // Expected layout (bottom-anchored):
        //   <user_row>:     "> 你好啊"
        //   <user_row + 1>: blank (UiLine::User's spacer)
        //   <user_row + 2>: "Hello world"  ← replaced spinner in-place
        //
        // Critical invariant: exactly ONE blank row between them.
        // No extra gap would mean 2 consecutive blanks.
        assert_eq!(
            hello_row - user_row,
            2,
            "expected 1 spacer row between user and assistant, got {} \
             rows gap:\n{}",
            hello_row.saturating_sub(user_row).saturating_sub(1),
            vterm.dump()
        );
    }

    /// Diagnostic: realistic flow — User → idle InputPrompt (sent
    /// BEFORE the first spinner tick to mirror the on_submit
    /// transition) → multiple spinner ticks → assertion on grid
    /// layout. User reported TWO blanks between `> 你好` and
    /// `● Pondering` — spec says there should be exactly ONE.
    #[test]
    fn retained_user_then_spinner_has_exactly_one_blank_between() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        r.render(UiLine::User("hi-from-user".into()));
        // on_submit in the real app triggers a render pass before
        // the first spinner tick lands — simulate that here.
        r.flush_deferred();
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Pondering".into(),
            status: status.clone(),
            menu: None,
            attachments: Vec::new(),
        });
        // Several animation ticks, then a final flush.
        for frame in ["⠙", "⠹", "⠸", "⠼"] {
            r.render(UiLine::StreamingBox {
                buf: String::new(),
                cursor_byte: 0,
                frame,
                label: "Pondering".into(),
                status: status.clone(),
                menu: None,
                attachments: Vec::new(),
            });
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let user_row = (0..24)
            .find(|r| vterm.row_text(*r).contains("hi-from-user"))
            .unwrap_or_else(|| panic!("user echo missing:\n{}", vterm.dump()));
        let spin_row = (0..24)
            .find(|r| vterm.row_text(*r).contains("Pondering"))
            .unwrap_or_else(|| panic!("spinner missing:\n{}", vterm.dump()));

        assert_eq!(
            spin_row - user_row,
            2,
            "expected exactly 1 blank row between user message and \
            spinner, got {} blank row(s):\n{}",
            spin_row.saturating_sub(user_row).saturating_sub(1),
            vterm.dump()
        );
    }

    /// If the turn ends with NO text output (just an empty input
    /// prompt arrives after the spinner), the spinner must also
    /// disappear. User's view: the in-progress indicator was
    /// transient; once the render state moves on, no residue remains.
    #[test]
    fn retained_input_prompt_clears_live_spinner() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Pondering".into(),
            status: status.clone(),
            menu: None,
            attachments: Vec::new(),
        });
        // Directly back to input with no assistant output between.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let has_spinner = vterm.any_row(|row| row.contains("⠋") && row.contains("Pondering"));
        assert!(
            !has_spinner,
            "spinner still visible after returning to input prompt:\n{}",
            vterm.dump()
        );
    }

    /// Markdown inline: `**bold**` + `` `code` `` rendered in
    /// the assistant-text stream. Grid inspects specific cells to
    /// confirm bold and bright-white fg survived the markdown → cells →
    /// serialize → vte parse round-trip.
    #[test]
    fn retained_markdown_inline_styles_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::AssistantText(
            "Hello **bold** and `code` here\n".into(),
        ));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let row_idx = (0..vterm.height() as usize)
            .find(|&r| vterm.row_text(r).contains("Hello bold and code here"))
            .unwrap_or_else(|| panic!("inline markdown text missing\ndump:\n{}", vterm.dump()));
        let row_text = vterm.row_text(row_idx);
        // 'b' of "bold" — the '*' markers are consumed. With
        // `  Hello **bold** and`, after markdown render it becomes
        // `  Hello bold and …`. Locate 'b' of "bold" and assert
        // its cell is bold.
        let bold_pos = row_text
            .find("bold")
            .expect("expected 'bold' in rendered text");
        let cell = vterm.cell_at(row_idx, bold_pos);
        assert!(
            cell.bold,
            "bold cell at col {} should be bold: {:?}\ndump:\n{}",
            bold_pos,
            cell,
            vterm.dump()
        );
        // Inline code: bold + bright cyan (SGR 96). The markdown crate
        // now colours inline code the same as headings and code-block
        // chrome, using the 16-colour SGR palette so the terminal theme
        // remaps the actual shade. In CellStyle this arrives as
        // `Color::Cyan` (crossterm's name for SGR 96 / bright cyan).
        let code_pos = row_text
            .find("code")
            .expect("expected 'code' in rendered text");
        let code_cell = vterm.cell_at(row_idx, code_pos);
        assert!(
            code_cell.bold,
            "inline code cell should be bold: {:?}",
            code_cell
        );
        assert_eq!(
            code_cell.fg,
            Some(Color::Cyan),
            "inline code cell must carry bright cyan fg: {:?}",
            code_cell
        );
    }

    /// Plain assistant paragraphs must retain their 2-col indent even
    /// after symbol-bearing rows move to col 0. Regression guard for
    /// the hierarchy: symbols at col 0, prose at col 2.
    #[test]
    fn retained_assistant_paragraph_indent_preserved() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::AssistantText("hello world\n".into()));
        r.render(UiLine::TurnComplete);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let row_idx = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("hello world"))
            .unwrap_or_else(|| panic!("assistant text row missing\ndump:\n{}", vterm.dump()));
        assert_eq!(vterm.cell_at(row_idx, 0).ch, ' ', "col 0 must be blank");
        assert_eq!(vterm.cell_at(row_idx, 1).ch, ' ', "col 1 must be blank");
        assert_eq!(
            vterm.cell_at(row_idx, 2).ch,
            'h',
            "assistant text must start at col 2, got row: {:?}",
            vterm.row_text(row_idx)
        );
    }

    /// Regression: user reports bot_rule row visibly shortens when
    /// the input wraps from 1 line to 2 lines. Hypothesis: diff
    /// spurious-skips the bot_rule row, or paint_body/footer
    /// miscomputes bot_rule_row and overwrites it.
    ///
    /// Direct assertion: after wrapping, inspect Screen.prev_cells
    /// (which is "what we just emitted") — every column in the
    /// bot_rule row must contain either a PAD_COL blank or a '─'.
    #[test]
    fn retained_bot_rule_full_width_after_wrap() {
        let (mut r, _buf) = new_capturing(40, 24);
        let status = status_basic();
        // Short input → 1-row middle.
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Long input → 2-row middle.
        let long: String = std::iter::repeat('中').take(40).collect();
        r.render(UiLine::InputPrompt {
            buf: long.clone(),
            cursor_byte: long.len(),
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Inspect the newly-emitted frame (prev_cells after swap).
        let h = r.screen.height() as usize;
        let footer_rows = r.current_footer_rows();
        let footer_top = h - footer_rows;
        // Layout: top_rule + middle×N + bot_rule + status (spinner no
        // longer reserves a footer row — lives in body now).
        // With 2-row middle: bot_rule at footer_top + 1 + 2 = footer_top + 3
        // text_budget = w - 2 ("> " prefix) = 38 for w=40.
        let (lines, _, _) = crate::width::wrap_with_cursor(&long, 40 - 2, long.len());
        assert!(lines.len() >= 2, "test setup: expected wrap");
        let bot_rule_row = footer_top + 1 + lines.len();
        let prev_cells = r.screen.prev_cells_for_test();
        let row_cells = &prev_cells[bot_rule_row];

        // Rule is flush-left/right now — every col 0..w is '─'.
        for (col, cell) in row_cells.iter().enumerate() {
            assert_eq!(
                cell.ch, '─',
                "col {} expected '─', got {:?} (rule short!)",
                col, cell
            );
        }
    }

    /// Regression for "login 后 输入内容过长不自动换行" report.
    /// User observed a single long-line input not wrapping — turned
    /// out the buffer was 202 display cols vs the 203-col budget, so
    /// legit 1-row. This test pins down that an input CLEARLY past
    /// the budget produces a multi-row footer, and the cursor
    /// lives in the LAST middle row (not the first).
    #[test]
    fn retained_long_input_wraps_to_multi_row_footer() {
        // Small screen so wrap happens without massive test data.
        // text_budget = width - 6 = 34, so any input > 34 cols wraps.
        let (mut r, _buf) = new_capturing(40, 24);
        // 40 CJK characters = 80 display cols → wraps to 3 rows (cols
        // 0..33, 34..67, 68..79). Each row has ~17 Chinese chars.
        let long: String = std::iter::repeat('中').take(40).collect();
        // cursor_byte = full UTF-8 length of the input (3 bytes per char × 40).
        r.render(UiLine::InputPrompt {
            buf: long.clone(),
            cursor_byte: long.len(),
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Directly query wrap result to verify wrap happened.
        let (lines, cursor_row, _cursor_col) =
            crate::width::wrap_with_cursor(&long, 40 - 6, long.len());
        assert!(
            lines.len() >= 2,
            "expected 2+ wrapped rows, got {} line(s): {:?}",
            lines.len(),
            lines
        );
        // Cursor should be in the LAST wrapped row (end of buffer).
        assert_eq!(
            cursor_row,
            lines.len() - 1,
            "cursor should be in last middle row"
        );

        // Now the integration check: the internal footer-rows count
        // must match wrap output. If paint_footer miscomputes, the
        // body area overlaps the multi-row middle.
        assert_eq!(
            r.current_footer_rows(),
            // 1 top rule + lines.len() + 1 bot rule + 0 menu + status(1)
            // (spinner moved to body — no longer reserves a footer row)
            1 + lines.len() + 1 + 1,
            "footer_rows must account for wrapped middle row count"
        );
    }

    /// Wide CJK input end-to-end: render "你是谁" from empty, assert
    /// emit stream contains the three glyphs consecutively (no
    /// cursor-drift desync between them).
    #[test]
    fn retained_wide_char_input_keeps_all() {
        let (mut r, buf) = new_capturing(80, 24);
        let status = status_basic();
        r.render(UiLine::InputPrompt {
            buf: "".into(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        buf.lock().unwrap().clear();

        r.render(UiLine::InputPrompt {
            buf: "你是谁".into(),
            cursor_byte: 9,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let stream_bytes = std::mem::take(&mut *buf.lock().unwrap());
        let stream = String::from_utf8_lossy(&stream_bytes).to_string();
        assert!(
            stream.contains("你是谁"),
            "wide chars not consecutive in retained emit stream:\n{}",
            stream
        );
    }

    /// Mac Terminal.app drops bytes mid-sequence when a single
    /// `write_all` carries ~1KB+ of mixed CSI/SGR/UTF-8 — observed as
    /// "bot_rule row shortens" after a big cold-start paint. The
    /// workaround in `flush_deferred` splits emits into 512 B chunks.
    /// Regression: a cold-start full frame (welcome + footer +
    /// menu open) must produce > 1 write call, with every chunk
    /// except the last sized exactly 512 bytes.
    #[test]
    fn retained_large_frame_splits_into_512b_chunks() {
        let (mut r, chunks) = new_chunk_counting(80, 24);
        let status = status_basic();

        // Build up a painted frame with welcome + open menu so the
        // cold-start emit is comfortably over 512 B. Welcome rows are
        // emitted via the body scrollback path (one write_all each),
        // so we reset the chunk tally after that stage and measure
        // only the footer paint — that's the one `flush_deferred`
        // splits into 512 B chunks.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        chunks.lock().unwrap().clear();
        let items: Vec<(String, String)> = vec![
            ("model".into(), "Switch model".into()),
            ("provider".into(), "Add provider".into()),
            ("session".into(), "New session".into()),
            ("resume".into(), "Resume session".into()),
        ];
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items,
                selected: 0,
                kind: crate::render::MenuKind::SlashCommand,
            }),
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();

        let sizes = chunks.lock().unwrap().clone();
        let total: usize = sizes.iter().sum();
        assert!(
            total > 512,
            "test needs a > 512 B frame to exercise chunking; got {} B (sizes: {:?})",
            total,
            sizes
        );
        assert!(
            sizes.len() > 1,
            "large frame must split into >1 write ({} B in one call)\nsizes: {:?}",
            total,
            sizes
        );
        // At least one chunk must be exactly 512 B — that's the
        // signature of the chunking loop actually firing on the main
        // diff payload. Small preamble writes (DECSTBM setup, cursor
        // moves emitted via separate `write!` calls outside the loop)
        // legitimately appear as their own sub-512 chunks.
        assert!(
            sizes.iter().any(|&s| s == 512),
            "expected at least one 512 B chunk from the chunking loop; sizes: {:?}",
            sizes
        );
        assert!(
            sizes.iter().all(|&s| s <= 512),
            "no chunk may exceed 512 B (sizes: {:?})",
            sizes
        );
    }

    /// Small frames must NOT chunk — single `write` per flush keeps
    /// syscall count minimal on the steady-state keystroke path.
    #[test]
    fn retained_small_frame_single_write() {
        let (mut r, chunks) = new_chunk_counting(80, 24);
        let status = status_basic();
        // Warm up so prev_cells matches.
        r.render(UiLine::InputPrompt {
            buf: "h".into(),
            cursor_byte: 1,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        chunks.lock().unwrap().clear();

        // Single keystroke — delta ≪ 512 B.
        r.render(UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let sizes = chunks.lock().unwrap().clone();
        assert_eq!(
            sizes.len(),
            1,
            "steady-state keystroke should be one write (sizes: {:?})",
            sizes
        );
        assert!(
            sizes[0] < 512,
            "keystroke delta should be well under 512 B (got {} B)",
            sizes[0]
        );
    }

    /// After `/clear` (renderer.clear_screen + re-render Welcome),
    /// the welcome must reappear on the grid. Previous bug: the
    /// immediate-mode renderer's diff cache was left intact by
    /// `clear_screen`, so the next welcome paint saw prev=welcome
    /// (stale), emitted no diff, and the terminal stayed blank.
    /// Retained mode closes this hole by blowing away the whole
    /// Screen model inside `clear_screen` — this test pins that
    /// behaviour.
    #[test]
    fn retained_clear_screen_then_welcome_renders_via_vterm() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Initial welcome.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            (0..24).any(|row| vterm.row_text(row).contains("AtomCode")),
            "baseline welcome missing:\n{}",
            vterm.dump()
        );

        // /clear — wipe terminal + re-render welcome. Note the
        // `clear_screen` call wipes state but doesn't repaint; the
        // next Welcome + flush does.
        r.clear_screen();
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Welcome must be back.
        let still_has = (0..24)
            .filter(|row| vterm.row_text(*row).contains("AtomCode"))
            .count();
        assert_eq!(
            still_has,
            1,
            "after /clear the welcome must appear exactly once (not 0, not 2+):\n{}",
            vterm.dump()
        );
    }

    /// `resume_from_external` (OAuth browser return, `/shell` exit)
    /// must (1) emit `\x1b[2J\x1b[H` to clear whatever the child
    /// process left on screen, and (2) invalidate the Screen cache
    /// so the next paint is a cold-start full repaint — otherwise
    /// the diff would skip every cell that happens to match
    /// prev_cells and the terminal would stay blank with a stale
    /// cache believing everything is fine.
    #[test]
    fn retained_resume_from_external_clears_and_forces_repaint() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Paint welcome first, drain so vterm + terminal state agree.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            (0..24).any(|row| vterm.row_text(row).contains("AtomCode")),
            "baseline welcome missing:\n{}",
            vterm.dump()
        );

        // Simulate the child process scribbling garbage on the
        // terminal — vterm feeds bytes only from the renderer's
        // sink, so we feed the "garbage" directly to vterm to
        // mimic a post-child state where on-screen content no
        // longer matches renderer's prev_cells.
        vterm.feed(b"\x1b[1;1H*** child process noise ***\r\n");
        assert!(
            vterm.row_text(0).contains("child process noise"),
            "setup: child-noise didn't land on vterm:\n{}",
            vterm.dump()
        );

        // Clear capture buffer so we can observe ONLY the bytes
        // emitted by resume_from_external + the next flush.
        buf.lock().unwrap().clear();
        r.resume_from_external();
        let resume_bytes = buf.lock().unwrap().clone();
        let resume_str = String::from_utf8_lossy(&resume_bytes);
        // Resume now uses per-row CUP+EL instead of ED (iTerm2 3.5+
        // observed to ignore `\x1b[2J` under certain states). Assert
        // the equivalent semantics: at least one EL landed AND the
        // cursor homes. The real behavioral check (no stale child
        // noise) runs at the end of this test.
        assert!(
            resume_str.contains("\x1b[K") && resume_str.contains("\x1b[H"),
            "resume must emit per-row EL + home: {:?}",
            resume_str
        );
        drain_into_vterm(&buf, &mut vterm);

        // After resume the next render must fully repaint against
        // blank prev_cells — verify by rendering the SAME welcome
        // content as before (so a naive cache would emit zero
        // bytes) and asserting it still produces a non-trivial
        // emit that restores AtomCode on the grid.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            (0..24).any(|row| vterm.row_text(row).contains("AtomCode")),
            "after resume_from_external the next paint must restore welcome (full repaint, not diff-skip):\n{}",
            vterm.dump()
        );
        assert!(
            !vterm.row_text(0).contains("child process noise"),
            "resume must erase child-process garbage at row 0:\n{}",
            vterm.dump()
        );
    }

    /// Regression for the "/ then Esc" ghost. With menu open the
    /// footer is taller so the bottom-anchored welcome paints at
    /// rows A..B. When the menu closes the footer shrinks and the
    /// welcome paints at rows A+k..B+k (further down). If the
    /// geometry-change path invalidates prev_cells without also
    /// erasing the terminal, the diff against blank-prev skips
    /// blank cells in the new frame — so the old welcome at rows
    /// A..A+k-1 stays on screen as a ghost underneath the fresh
    /// paint.
    #[test]
    fn retained_menu_close_leaves_no_welcome_ghost() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Initial welcome (no menu). Footer = 4 rows (top_rule /
        // middle / bot_rule / status). Welcome 8 rows bottom-anchored
        // at rows 12..=19 (0-idx). Banner = title + path + model +
        // blank + 3 hint rows + trailing blank.
        r.render(UiLine::Welcome {
            model: "glm-5".into(),
            working_dir: "~/project/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Open menu ("/" pressed). Footer grows by 4 rows (menu) →
        // 8 rows. Welcome (8 rows) paints at 0-idx rows 8..=15.
        let items: Vec<(String, String)> = vec![
            ("model".into(), "Switch model".into()),
            ("provider".into(), "Add provider".into()),
            ("session".into(), "New session".into()),
            ("resume".into(), "Resume session".into()),
        ];
        r.render(UiLine::InputPrompt {
            buf: "/".into(),
            cursor_byte: 1,
            menu: Some(MenuPayload {
                items: items.clone(),
                selected: 0,
                    kind: crate::render::MenuKind::SlashCommand,
            }),
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Close menu (Esc). Footer shrinks back to 4, welcome
        // re-paints via `ensure_scroll_region`'s grew branch →
        // back to 0-idx rows 12..=19.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Welcome brand at row 12 post-close. Row 8 (where brand
        // lived mid-menu) must be blank now — the zombie-zone erase
        // must have cleaned it.
        assert!(
            vterm.row_text(12).contains("AtomCode"),
            "menu-close: welcome brand missing at row 12:\n{}",
            vterm.dump()
        );
        assert!(
            !vterm.row_text(8).contains("AtomCode"),
            "menu-close: row 8 still shows ghost welcome brand:\n{}",
            vterm.dump()
        );
        // Same for cwd row (was 0-idx row 9 mid-menu, moves to 13).
        assert!(
            !vterm.row_text(9).contains("project"),
            "menu-close: row 9 still shows ghost cwd:\n{}",
            vterm.dump()
        );
    }

    /// Regression for user report: after `/model` switched providers,
    /// scrolling up showed the welcome banner + prior messages
    /// duplicated in scrollback. Root cause: `/model` changes the
    /// status-line text, which can change the footer height (status
    /// wraps, or spinner/menu rows differ between frames). When
    /// `current_footer_rows()` shifts, `ensure_scroll_region`'s
    /// shrunk/grew branches clear the viewport and re-emit every
    /// cached body row through `emit_body_line_inner` — which uses
    /// `\n` at the region bottom, scrolling the top row into
    /// terminal scrollback. Any cached body row that had already
    /// entered scrollback during its original emit now enters a
    /// second time: a duplicate the user sees on scroll-up.
    ///
    /// Repro: fill body past the viewport so a known welcome line
    /// lives in scrollback once, then change the footer height by
    /// swapping in an input long enough to wrap the middle to 2+
    /// rows. The hint line must still appear exactly once in
    /// scrollback afterwards — the repaint must not re-scroll it.
    #[test]
    fn retained_footer_growth_does_not_duplicate_scrollback() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Welcome (7 body rows) + 20 User echoes (2 rows each =
        // 40 body rows). Total 47 rows pushed; body region bottom
        // with a 1-line-input footer is < 20, so ~27 rows are
        // already in terminal scrollback via the normal emit path.
        r.render(UiLine::Welcome {
            model: "MiniMax-M2.7".into(),
            working_dir: "~/Documents/workspace/atomcode".into(),
        });
        for i in 0..20 {
            r.render(UiLine::User(format!("msg-{:03}", i)));
        }
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Fingerprint: welcome hint is unique and we pushed it
        // early enough that it's sitting in scrollback by now.
        let hint = "to add a custom model";
        let count_hint = |vt: &crate::test_term::VirtualTerminal| {
            vt.scrollback_texts()
                .iter()
                .filter(|row| row.contains(hint))
                .count()
        };
        assert_eq!(
            count_hint(&vterm),
            1,
            "baseline: hint should sit in scrollback exactly once \
             after normal emits (got {}):\n{}",
            count_hint(&vterm),
            vterm.scrollback_texts().join("\n")
        );
        let sb_before = vterm.scrollback_len();

        // Footer height change: long buffer wraps the middle to 3
        // rows (text budget = 80 - 6 = 74 cols; 200 'x' → 3 rows).
        // body_bottom shrinks → ensure_scroll_region's shrunk branch
        // fires. Before the fix, this re-emits every cached body
        // row via `\n`-scroll, pushing overflow into scrollback a
        // second time.
        let long: String = "x".repeat(200);
        r.render(UiLine::InputPrompt {
            buf: long.clone(),
            cursor_byte: long.len(),
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        assert_eq!(
            count_hint(&vterm),
            1,
            "footer growth duplicated welcome hint in scrollback \
             (got {} copies):\nscrollback:\n{}",
            count_hint(&vterm),
            vterm.scrollback_texts().join("\n")
        );
        // Broader sanity: no body row should have been pushed into
        // scrollback by the repaint itself. The footer grew by N
        // rows, which means the visible body shrank by N rows — the
        // terminal's native region-shrink does not push rows to
        // scrollback, only LFs at the bottom do. So the only way
        // scrollback_len grew here is via the buggy re-emit.
        assert_eq!(
            vterm.scrollback_len(),
            sb_before,
            "footer growth pushed {} extra rows into scrollback; \
             repaint must use absolute positioning, not LF-scroll",
            vterm.scrollback_len() - sb_before
        );
    }

    /// Regression for user report: after `/quit`, the newest answer
    /// rows that were still visible above the fixed footer vanished
    /// from host-terminal history. They had never naturally scrolled
    /// into native scrollback, and shutdown wiped the viewport.
    #[test]
    fn retained_shutdown_promotes_visible_body_tail_to_scrollback() {
        let (mut r, buf) = new_capturing(80, 12);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 12);
        let status = status_basic();

        r.render(UiLine::User("show config routes".into()));
        r.render(UiLine::CommandOutput(
            "GET /config\nPOST /config/reload\nvisible-bottom-answer\n".into(),
        ));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        assert!(
            !vterm
                .scrollback_texts()
                .iter()
                .any(|row| row.contains("visible-bottom-answer")),
            "baseline should keep the newest visible answer out of scrollback until shutdown"
        );

        r.shutdown();
        drain_into_vterm(&buf, &mut vterm);

        assert!(
            vterm
                .scrollback_texts()
                .iter()
                .any(|row| row.contains("visible-bottom-answer")),
            "shutdown must preserve the visible body tail in scrollback:\n{}",
            vterm.scrollback_texts().join("\n")
        );
    }

    /// Regression for user report: on first startup the welcome
    /// banner rendered TWICE — once at the top of the viewport
    /// (pushed into scrollback, no input box) and once at the bottom
    /// above the input box. Root cause: `ensure_scroll_region` used
    /// `\x1b[2J` to wipe the viewport before re-painting the body.
    /// macOS Terminal.app and iTerm2 (and xterm with `cbScrollback`)
    /// copy every non-blank visible row into scrollback when
    /// processing ED — so the 6 welcome rows painted during the
    /// initial body emit were promoted into scrollback the moment
    /// the first InputPrompt render caused the footer to grow by
    /// 1 row (status line appears → body_bottom shrinks by 1).
    ///
    /// The repaint must never emit ED — per-row EL (`\x1b[K`) at
    /// absolute positions is safe on every terminal and achieves
    /// the same visible result without the scrollback side-channel.
    #[test]
    fn retained_first_startup_does_not_push_welcome_to_scrollback() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        // Model the terminal's ED-promotes-to-scrollback behaviour —
        // the specific mode the user's terminal is running under.
        vterm.set_ed_promotes_to_scrollback(true);

        // Minimal first-startup sequence: welcome then the first
        // InputPrompt. The InputPrompt carries a non-empty status
        // (model/cwd) so `current_footer_rows` grows from 4 (no
        // status) to 5, which trips the repaint branch.
        r.render(UiLine::Welcome {
            model: "z-ai/glm-5".into(),
            working_dir: "~/Documents/workspace/atomcode".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Welcome fingerprint: `/codingplan` is unique to the welcome
        // hint row and is a single non-wrapping token, so it gives a
        // stable single-row marker even when the combined hint line
        // soft-wraps at narrower widths. Must appear exactly once in
        // the *visible* viewport and zero times in scrollback.
        let hint = "/codingplan";
        let visible_count = (0..24)
            .filter(|r| vterm.row_text(*r).contains(hint))
            .count();
        let sb_count = vterm
            .scrollback_texts()
            .iter()
            .filter(|row| row.contains(hint))
            .count();
        assert_eq!(
            visible_count,
            1,
            "welcome hint should be visible exactly once (got {}):\n{}",
            visible_count,
            vterm.dump()
        );
        assert_eq!(
            sb_count,
            0,
            "first-startup footer transition promoted welcome into \
             scrollback ({} copies); repaint must not emit ED:\n\
             scrollback:\n{}",
            sb_count,
            vterm.scrollback_texts().join("\n")
        );
    }

    /// Regression for user report: Shift+Enter in the input followed
    /// by delete leaves an extra rule line on screen. Root cause:
    /// Shift+Enter grows middle from 1 to 2 rows (body bottom -1);
    /// delete shrinks it back (body bottom +1, a GROW transition).
    /// In the new layout the OLD top-rule row lands on the new
    /// spinner slot — which paint_footer writes as a blank row when
    /// no spinner is active. `screen.invalidate()` zeroes prev_cells,
    /// so cell diff sees blank→blank at that row and emits nothing;
    /// the old rule glyphs persist on screen, stacked directly above
    /// the new top rule.
    ///
    /// Fix: repaint must explicitly erase every row in the union of
    /// old and new footer regions before the cell diff runs — EL is
    /// row-local so it doesn't leak content into scrollback.
    #[test]
    fn retained_middle_grow_then_shrink_leaves_no_ghost_rule() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // State A: 1-row middle (baseline).
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // State B: shift+enter — 2-row middle. Buf "\n" wraps to
        // 2 lines per `wrap_with_cursor`. Footer +1, body -1.
        r.render(UiLine::InputPrompt {
            buf: "\n".into(),
            cursor_byte: 1,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // State C: delete back to empty. Body grows 1 row. This is
        // the transition that exposes the ghost rule.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // The input frame has exactly one top rule and one bot rule.
        // Each rule row is a full-width run of '─' (U+2500) with no
        // other glyphs. Count rows whose content is ONLY rule cells
        // — there must be exactly 2 after a clean grow+shrink. A
        // ghost from the old layout pushes this to 3.
        let rule_rows = (0..24)
            .filter(|r| {
                let txt = vterm.row_text(*r);
                let trimmed = txt.trim_end();
                !trimmed.is_empty() && trimmed.chars().all(|c| c == '\u{2500}')
            })
            .count();
        assert_eq!(
            rule_rows,
            2,
            "expected 2 rule rows (top + bot), got {} — grow \
             transition left a ghost:\n{}",
            rule_rows,
            vterm.dump()
        );
    }

    /// Live-group flow:
    /// 1. ToolGroupRender pushes header + 3 child rows
    /// 2. ToolGroupChildUpdate on the MIDDLE child rewrites that row
    ///    in place via CUP — peers (rows above/below) untouched.
    ///
    /// Pinpoints CC-style "✓ trickles into existing row" behavior so
    /// any future regression (e.g. accidental `push_body_row` for
    /// child updates) gets caught.
    #[test]
    fn tool_group_render_then_child_update_in_place() {
        use crate::render::ToolGroupChild;
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);

        r.render(UiLine::ToolGroupRender {
            batch_id: "b1".into(),
            header: "▸ Running 3 read_file calls in parallel".into(),
            children: vec![
                ToolGroupChild {
                    call_id: "c1".into(),
                    text: "  ↳ Read File foo.rs".into(),
                },
                ToolGroupChild {
                    call_id: "c2".into(),
                    text: "  ↳ Read File bar.rs".into(),
                },
                ToolGroupChild {
                    call_id: "c3".into(),
                    text: "  ↳ Read File baz.rs".into(),
                },
            ],
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let dump_before = vterm.dump();
        assert!(
            dump_before.contains("Running 3 read_file"),
            "header missing:\n{}",
            dump_before
        );
        assert!(dump_before.contains("Read File foo.rs"));
        assert!(dump_before.contains("Read File bar.rs"));
        assert!(dump_before.contains("Read File baz.rs"));
        // No ✓ yet — every child still shows its initial dispatched row.
        assert!(
            !dump_before.contains("✓"),
            "no checkmark expected pre-update:\n{}",
            dump_before
        );

        // In-place update of the middle child — CUPs to that row and
        // rewrites without pushing a new body row.
        r.render(UiLine::ToolGroupChildUpdate {
            batch_id: "b1".into(),
            call_id: "c2".into(),
            new_text: "  ↳ ✓ Read File bar.rs".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let dump_after = vterm.dump();
        assert!(
            dump_after.contains("✓ Read File bar.rs"),
            "✓ on bar.rs row missing after update:\n{}",
            dump_after
        );
        // Other two children untouched — exactly one ✓ in the dump.
        let check_count = dump_after.matches("✓").count();
        assert_eq!(
            check_count, 1,
            "expected exactly 1 ✓ (middle child only); got {}:\n{}",
            check_count, dump_after
        );
    }

    /// Foreign body push between ToolGroupRender and ChildUpdate
    /// freezes the group. Subsequent updates must no-op (rather than
    /// CUP-rewrite some unrelated row that took the child's screen
    /// position). Model still has the ToolResult — only the visual
    /// ✓ light-up is dropped, which is the safe outcome.
    #[test]
    fn tool_group_freezes_after_unrelated_body_push() {
        use crate::render::ToolGroupChild;
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);

        r.render(UiLine::ToolGroupRender {
            batch_id: "b1".into(),
            header: "▸ batch header".into(),
            children: vec![
                ToolGroupChild {
                    call_id: "c1".into(),
                    text: "  ↳ child one".into(),
                },
                ToolGroupChild {
                    call_id: "c2".into(),
                    text: "  ↳ child two".into(),
                },
            ],
        });
        // Foreign push — freezes the group.
        r.render(UiLine::CommandOutput("foreign output line".into()));
        // This update would have rewritten child1 in place, but the
        // group is now frozen → must be a no-op.
        r.render(UiLine::ToolGroupChildUpdate {
            batch_id: "b1".into(),
            call_id: "c1".into(),
            new_text: "  ↳ ✓ child one (should NOT appear)".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let dump = vterm.dump();
        assert!(
            dump.contains("foreign output line"),
            "foreign push should still show:\n{}",
            dump
        );
        assert!(
            !dump.contains("(should NOT appear)"),
            "frozen group must not apply child update; got:\n{}",
            dump
        );
        assert!(
            !dump.contains("✓ child one"),
            "no ✓ should appear on the child after freeze:\n{}",
            dump
        );
    }

    /// `attachments` from `UiLine::InputPrompt` paints a `└ [Image #N]`
    /// preview row between the bot_rule and the menu — same string the
    /// post-submit body echoes via `UiLine::ImageAttachment`. This is
    /// the only visual signal users have pre-submit that a paste
    /// actually attached an image (vs `[Image #N]` that they typed as
    /// literal text).
    #[test]
    fn input_prompt_attachments_render_preview_rows() {
        let (mut r, buf) = new_capturing(80, 24);
        r.render(UiLine::InputPrompt {
            buf: "see [Image #3] please".into(),
            cursor_byte: 21,
            menu: None,
            status: status_basic(),
            attachments: vec![3],
        });
        r.flush_deferred();
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        drain_into_vterm(&buf, &mut vterm);
        let dump = vterm.dump();
        assert!(
            dump.contains("└ [Image #3]"),
            "preview row must render the muted `└ [Image #N]` echo string; got:\n{}",
            dump
        );
    }

    /// Empty `attachments` keeps the footer at its prior height — no
    /// blank preview row, no off-by-one in `current_footer_rows()`.
    /// Regression guard: an earlier draft would have incremented the
    /// row count even when the vec was empty, pushing the input box
    /// up by one row whenever `attachments` was wired through.
    #[test]
    fn input_prompt_no_attachments_keeps_footer_height() {
        let (mut r, _) = new_capturing(80, 24);
        r.render(UiLine::InputPrompt {
            buf: "before".into(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        let baseline = r.current_footer_rows();
        r.render(UiLine::InputPrompt {
            buf: "no images here".into(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        assert_eq!(
            r.current_footer_rows(),
            baseline,
            "empty attachments must not change footer height"
        );
    }

    /// Footer height grows by exactly one row per attachment, so the
    /// body anchor (computed from `current_footer_rows()`) tracks the
    /// preview rows. Without this, a user with two attachments would
    /// see the topmost body row clipped under the input box.
    #[test]
    fn input_prompt_each_attachment_adds_one_row() {
        let (mut r, _) = new_capturing(80, 24);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        let baseline = r.current_footer_rows();
        r.render(UiLine::InputPrompt {
            buf: "[Image #1] [Image #2]".into(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: vec![1, 2],
        });
        assert_eq!(
            r.current_footer_rows(),
            baseline + 2,
            "two attachments must add exactly two preview rows"
        );
    }

    /// Regression: SGR (`\x1b[31m…\x1b[39m`) embedded in a
    /// `UiLine::CommandOutput` payload — emitted by the `/codingplan`
    /// SetupReport for locked-model rows — must reach the cell grid
    /// as a `CellStyle::fg = Some(DarkRed)` span rather than landing
    /// as literal `^[[31m` characters. Without the SGR-aware
    /// CommandOutput path in retained-mode, locked rows render
    /// without the colour cue, defeating the visual signal the user
    /// asked for.
    #[test]
    fn retained_command_output_renders_sgr_colour() {
        let (mut r, _buf) = new_capturing(80, 24);
        // Construct the exact byte sequence the `Msg::CpLocked`
        // template produces: red-fg open, visible content, default-fg
        // close. PAD_COL (2 spaces) on the left is added by
        // push_body_text_sgr; the template-level 6-space indent stays
        // on the visible side.
        let line = "      \x1b[31m✗ GLM-5.1  (requires Pro plan or higher)\x1b[39m\n";
        r.render(UiLine::CommandOutput(line.into()));

        // Find the row containing the locked-model name and check
        // every glyph cell up to the closing SGR is DarkRed.
        let mut found_red = false;
        for row in &r.body_lines {
            let text: String = row.iter().map(|c| c.ch).collect();
            if text.contains("GLM-5.1") {
                for cell in row {
                    // Skip the leading PAD_COL spaces (no colour applied
                    // before SGR fires) — only assert the styled span.
                    if cell.ch == ' ' && cell.style.fg.is_none() {
                        continue;
                    }
                    assert_eq!(
                        cell.style.fg,
                        Some(Color::DarkRed),
                        "cell '{}' in locked row must carry DarkRed fg, got {:?}",
                        cell.ch, cell.style.fg,
                    );
                }
                found_red = true;
                break;
            }
        }
        assert!(
            found_red,
            "no row containing 'GLM-5.1' found in body_lines:\n{:?}",
            r.body_lines
                .iter()
                .map(|row| row.iter().map(|c| c.ch).collect::<String>())
                .collect::<Vec<_>>()
        );

        // And the raw `^[[31m` characters must NOT appear as cells —
        // that's the bug we're guarding against.
        for row in &r.body_lines {
            let text: String = row.iter().map(|c| c.ch).collect();
            assert!(
                !text.contains("[31m"),
                "SGR bytes leaked into cells as literal text: {:?}",
                text,
            );
        }
    }

    /// Regression: after approving a bash tool call, the `● Bash(cmd)` row
    /// and the `└ [elapsed: …]` result row should be adjacent with no
    /// blank line between them. User reported a visible blank gap after
    /// pressing Y on the approval prompt.
    #[test]
    fn retained_approval_pop_then_result_no_blank_gap() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Seed a full frame so footer is painted.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Simulate: ToolCallStarted → inflight spinner for Bash
        r.render(UiLine::ToolCallInFlight {
            id: "call-1".into(),
            name: "Bash".into(),
            detail: "rm -f /tmp/test.txt".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Simulate: ApprovalNeeded → commit inflight to ● + show approval prompt
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-1".into()),
        });
        r.render(UiLine::ApprovalPrompt {
            tool: "Bash".into(),
            detail: "rm -f /tmp/test.txt".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // User presses Y → pop approval prompt
        r.pop_approval_prompt();
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Simulate: ToolCallResult arrives
        r.render(UiLine::AssistantLineBreak);
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-1".into()),
        });
        r.render(UiLine::ToolResult {
            success: true,
            summary: "[elapsed: 0.0s, exit: 0] (2 lines)".into(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Debug: print body_lines around the tool and result rows.
        let tool_idx = r.body_lines.iter().rposition(|row| {
            let text: String = row.iter().map(|c| c.ch).collect();
            text.contains("Bash") && text.contains("rm -f")
        }).expect("● Bash row should exist in body_lines");

        let result_idx = r.body_lines.iter().rposition(|row| {
            let text: String = row.iter().map(|c| c.ch).collect();
            text.contains("elapsed")
        }).expect("└ result row should exist in body_lines");

        eprintln!("body_lines around tool row:");
        for i in tool_idx.saturating_sub(2)..=result_idx+2 {
            if let Some(row) = r.body_lines.get(i) {
                let text: String = row.iter().map(|c| c.ch).collect();
                eprintln!("  [{}] {:?} (blank={})", i, text, row.is_empty());
            }
        }

        // Check body_lines: there should be no blank row between the
        // ● Bash row and the └ result row.
        assert_eq!(
            result_idx,
            tool_idx + 1,
            "result row should be immediately after tool row, but found gap.\n\
             body_lines around tool row:\n  {:?}\n  {:?}\n  {:?}",
            r.body_lines.get(tool_idx).map(|row| row.iter().map(|c| c.ch).collect::<String>()),
            r.body_lines.get(tool_idx + 1).map(|row| row.iter().map(|c| c.ch).collect::<String>()),
            r.body_lines.get(tool_idx + 2).map(|row| row.iter().map(|c| c.ch).collect::<String>()),
        );

        // Also check the virtual terminal: the ● Bash row and └ result row
        // should be on adjacent terminal rows with no blank row between them.
        eprintln!("vterm dump:\n{}", vterm.dump());
        let bash_term_row = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("Bash") && vterm.row_text(i).contains("rm"))
            .expect("Bash row should be on terminal");
        let result_term_row = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("elapsed"))
            .expect("result row should be on terminal");

        assert_eq!(
            result_term_row,
            bash_term_row + 1,
            "result should be on terminal row immediately below Bash row.\n\
             Bash row {}: {:?}\n\
             Row below: {:?}\n\
             Result row {}: {:?}\n\
             dump:\n{}",
            bash_term_row,
            vterm.row_text(bash_term_row),
            vterm.row_text(bash_term_row + 1),
            result_term_row,
            vterm.row_text(result_term_row),
            vterm.dump(),
        );
    }

    /// Regression: when a long Bash command wraps to multiple terminal
    /// rows, the inflight spinner `⠙ Bash(...)` may occupy 2+ body rows.
    /// After `ToolCallCommit` freezes it to `● Bash(...)`, the old
    /// spinner rows must all be erased — otherwise the user sees BOTH
    /// `⠙ Bash(...)` and `● Bash(...)` on screen at the same time.
    #[test]
    fn retained_commit_inflight_erases_all_spinner_rows() {
        // Use a narrow terminal so the command wraps to 2+ rows.
        let (mut r, buf) = new_capturing(40, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(40, 24);
        let status = status_basic();

        // Seed a full frame so footer is painted.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // ToolCallInFlight with a long command that wraps to 2 rows.
        let long_detail = "rm -rf /very/long/path/that/wraps/to/multiple/rows/on/40col/terminal";
        r.render(UiLine::ToolCallInFlight {
            id: "call-1".into(),
            name: "Bash".into(),
            detail: long_detail.into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Confirm the inflight spinner occupies more than 1 body row.
        assert!(
            r.inflight_tool_rows > 1,
            "inflight spinner should occupy multiple rows for a long command on 40-col terminal, \
             but inflight_tool_rows = {}",
            r.inflight_tool_rows,
        );

        // Now commit the inflight spinner (simulates ApprovalNeeded → ToolCallCommit).
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-1".into()),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Check body_lines: there should be exactly one row with "● Bash"
        // and NO row with a spinner glyph (⠙ or similar Braille pattern).
        let bash_rows: Vec<_> = r.body_lines.iter()
            .enumerate()
            .filter(|(_, row)| {
                let text: String = row.iter().map(|c| c.ch).collect();
                text.contains("Bash")
            })
            .collect();

        assert_eq!(
            bash_rows.len(),
            1,
            "there should be exactly 1 Bash row in body_lines, found {}:\n{:?}",
            bash_rows.len(),
            bash_rows.iter().map(|(i, row)| (i, row.iter().map(|c| c.ch).collect::<String>())).collect::<Vec<_>>(),
        );

        // The committed row should start with ● (U+25CF), not a spinner glyph.
        let (idx, bash_row) = bash_rows[0];
        let first_ch = bash_row.first().map(|c| c.ch).unwrap_or('\0');
        assert_eq!(
            first_ch, '\u{25cf}',
            "committed Bash row at index {} should start with ●, found '{}'",
            idx, first_ch,
        );

        // Check virtual terminal: no row should contain a Braille spinner
        // glyph (U+2800–U+28FF) alongside "Bash".
        for i in 0..vterm.height() as usize {
            let text = vterm.row_text(i);
            if text.contains("Bash") {
                let has_spinner = text.chars().any(|c| c >= '\u{2800}' && c <= '\u{28FF}');
                assert!(
                    !has_spinner,
                    "terminal row {} still has a spinner glyph alongside Bash: {:?}",
                    i, text,
                );
            }
        }
    }

    // --- width-aware truncation tests (Bug B) ---
    //
    // ToolGroup rows are forced to single terminal lines so child indices map
    // 1:1 with terminal positions for in-place CUP rewrites. Pre-fix the
    // truncators counted code points instead of display columns, so a row of
    // 30 汉字 (60 cols) on a 40-col screen never tripped the truncate branch
    // and the wide cells leaked past the screen edge — Screen::draw_row then
    // hard-cut mid-glyph with no `…` marker.

    #[test]
    fn build_one_row_cjk_does_not_overflow_screen() {
        // 30 汉字 = 60 display cols. Screen 40 → avail = 40 - PAD_COL = 38.
        // Row's summed cell widths must fit within avail.
        let text = "你".repeat(30);
        let row = build_one_row(&text, &CellStyle::default(), 40);
        let total_cols: usize = row.iter().map(|c| c.width as usize).sum();
        assert!(
            total_cols <= 38,
            "row width {} cols exceeds avail 38 (screen=40, PAD_COL=2)",
            total_cols
        );
    }

    #[test]
    fn truncate_body_str_uses_display_width_not_char_count() {
        // 50 汉字 = 100 display cols. Budget 10 means "≤10 display cols of
        // visible content"; the old `char_indices().nth(10)` cut after 10 code
        // points (= 20 cols) which still overflows narrow ToolGroup rows.
        let out = truncate_body_str(&"你".repeat(50), 10);
        let w = crate::width::display_width(&out);
        assert!(w <= 10, "output {} cols exceeds budget 10", w);
    }

    // --- SGR parser parity (Bug C) ---
    //
    // `CellStyle.faint` exists (cell.rs:48) and `cell::apply_sgr_params`
    // already honors SGR 2 + clears it on SGR 22. The retained.rs local
    // parser was missing both — commit 24b6dc04 switched the resumed
    // divider to `\x1b[2m`, but trusted output routed through this parser
    // would silently drop dim.

    #[test]
    fn apply_sgr_handles_faint_sgr_2() {
        let mut style = CellStyle::default();
        apply_sgr("2", &mut style);
        assert!(style.faint, "SGR 2 must set faint");
    }

    #[test]
    fn apply_sgr_22_clears_both_bold_and_faint() {
        let mut style = CellStyle {
            bold: true,
            faint: true,
            ..CellStyle::default()
        };
        apply_sgr("22", &mut style);
        // ECMA-48 22 = "normal intensity" — clears bold AND faint as a pair;
        // there's no per-attribute toggle for faint.
        assert!(!style.bold, "SGR 22 must clear bold");
        assert!(!style.faint, "SGR 22 must clear faint");
    }

    #[test]
    fn retained_body_lines_cap_is_5000_not_height_times_4() {
        let (mut r, _buf) = new_capturing(80, 24);
        // Push 5050 user lines (use a method that goes through push_body_row).
        for i in 0..5050 {
            r.render(UiLine::User(format!("line {}", i)));
        }
        assert_eq!(r.body_lines.len(), 5000, "body_lines should cap at 5000, got {}", r.body_lines.len());
    }

    #[test]
    fn retained_message_marks_tracked_on_user_push() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.render(UiLine::User("hi".into()));
        assert_eq!(r.message_marks.len(), 1);
        assert_eq!(r.message_marks[0].kind, crate::render::MarkKind::User);
    }

    #[test]
    fn retained_message_marks_decremented_on_drain() {
        let (mut r, _buf) = new_capturing(80, 24);
        // Each UiLine::User pushes 2 body rows (user text + blank spacer).
        // 5005 users → 10010 body rows. drain = 10010 - 5000 = 5010 rows from front.
        // Marks at line_idx < 5010 are dropped; the first surviving mark is at
        // original idx=5010, which normalises to 0 after subtracting the drain.
        for i in 0..5005 {
            r.render(UiLine::User(format!("line {}", i)));
        }
        // 5010 / 2 = 2505 marks dropped; 5005 - 2505 = 2500 survive.
        assert_eq!(r.message_marks.len(), 2500);
        assert_eq!(r.message_marks[0].line_idx, 0, "first surviving mark should point at body_lines[0] after drain");
    }

    #[test]
    fn retained_scroll_up_enters_view_mode() {
        let (mut r, _buf) = new_capturing(80, 24);
        for i in 0..30 {
            r.render(UiLine::User(format!("L{}", i)));
        }
        assert!(r.sticky_bottom);
        assert!(!r.view_mode);
        r.scroll_body(-3);
        assert!(r.view_mode, "scroll up must enter view_mode");
        assert!(!r.sticky_bottom);
    }

    #[test]
    fn retained_scroll_to_bottom_exits_view_mode() {
        let (mut r, _buf) = new_capturing(80, 24);
        for i in 0..30 {
            r.render(UiLine::User(format!("L{}", i)));
        }
        r.scroll_body(-5);
        assert!(r.view_mode);
        r.scroll_body_to_bottom();
        assert!(!r.view_mode);
        assert!(r.sticky_bottom);
    }

    #[test]
    fn retained_scroll_up_then_to_top_lands_at_zero() {
        let (mut r, _buf) = new_capturing(80, 24);
        for i in 0..30 {
            r.render(UiLine::User(format!("L{}", i)));
        }
        r.scroll_body_to_top();
        assert_eq!(r.viewport_top, 0);
        assert!(r.view_mode);
    }

    #[test]
    fn retained_view_mode_suppresses_terminal_writes() {
        let (mut r, buf) = new_capturing(80, 24);
        // Get into view_mode
        for i in 0..30 {
            r.render(UiLine::User(format!("L{}", i)));
        }
        r.scroll_body(-5);
        assert!(r.view_mode);
        let bytes_before = buf.lock().unwrap().len();
        // Push more content; terminal write count should not grow (view paint
        // is idempotent and we already painted in scroll_body).
        r.render(UiLine::User("after view".into()));
        // Snapshot to drop the lock before further mutation
        let new_bytes = buf.lock().unwrap()[bytes_before..].to_vec();
        let s = String::from_utf8_lossy(&new_bytes);
        assert!(!s.contains('\n'), "view_mode must NOT emit \\n scroll: {:?}", s);
        // body_lines should still grow.
        let non_empty = r.body_lines.iter().filter(|row| !row.is_empty()).count();
        assert!(non_empty >= 31, "expected body_lines to keep growing in view_mode, got {}", non_empty);
    }
}
