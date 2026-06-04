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

use std::fs::File;
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

/// Render context usage as `12.3k/131k tok` when both used and window
/// are known, or `12.3k tok` when only the used count is known (provider
/// hasn't reported its window yet, e.g. pre-config or fallback).
///
/// Unit ladder is k below 1M and m at/above 1M, so a 1_000_000-token
/// window reads as `1m` instead of the visually-confusing `1000k`. Round
/// values stay clean (`1m`, `131k`); non-round values get one decimal
/// (`1.5m`, `10.4k`).
fn format_ctx_usage(used: usize, window: usize) -> String {
    let used_label = format_tok_count(used, /*round_clean=*/ false);
    if window == 0 {
        format!("{} tok", used_label)
    } else {
        let window_label = format_tok_count(window, /*round_clean=*/ true);
        format!("{}/{} tok", used_label, window_label)
    }
}

/// Format a token count using k/m units. `round_clean=true` drops the
/// decimal when the value is an exact multiple of the unit (used for
/// the model's advertised window — `128_000` → `128k`, `1_000_000` →
/// `1m`). `round_clean=false` always emits one decimal at unit scale
/// (used for the live counter — `10_400` → `10.4k`, `1_500_000` → `1.5m`).
fn format_tok_count(n: usize, round_clean: bool) -> String {
    if n >= 1_000_000 {
        if round_clean && n % 1_000_000 == 0 {
            format!("{}m", n / 1_000_000)
        } else {
            format!("{:.1}m", (n as f64) / 1_000_000.0)
        }
    } else if n >= 1000 {
        if round_clean && n % 1000 == 0 {
            format!("{}k", n / 1000)
        } else if round_clean {
            format!("{:.0}k", (n as f64) / 1000.0)
        } else {
            format!("{:.1}k", (n as f64) / 1000.0)
        }
    } else {
        format!("{}", n)
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
        let w = crate::width::cell_char_width(c).unwrap_or(1);
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
    /// Number of `body_lines` front entries that have already been
    /// physically promoted to the host terminal's native scrollback via
    /// the bottom-row LF in `emit_body_line_inner`. Monotonically grows
    /// (one increment per overflow LF) and only resets in `reset()`.
    ///
    /// Why this exists: `body_lines` keeps the full history vector so
    /// `message_marks` / `welcome_line_count` / `live_group.child_indices`
    /// can index into it stably. But the VISIBLE window must never include
    /// rows whose copies already sit in native scrollback — otherwise a
    /// tail-pop (spinner clear, approval pop, ImageAttachment, inflight
    /// commit) shrinks `body_lines.len()`, lowers `start = len - cap`,
    /// and re-exposes a row whose `emit_body_line_inner` LF will then
    /// promote it to scrollback a SECOND time. Result: adjacent
    /// duplicates the user sees when scrolling up.
    ///
    /// `paint_body_into_cells` floors `start` at `scrolled_off` and the
    /// overflow check / target row both operate on the live window
    /// `body_lines.len() - scrolled_off`, which makes the visible
    /// region monotonically advance through `body_lines` regardless of
    /// tail mutations.
    scrolled_off: usize,
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
    /// Windows only: the STD_INPUT_HANDLE console mode value saved by
    /// `enable_conhost_mouse_capture`. Currently always `None` — mouse
    /// capture is intentionally disabled (see `with_writer` comment),
    /// so there's nothing to restore. Field retained because the
    /// suspend / shutdown paths still defensively call
    /// `restore_conhost_console_in_mode` if it ever became `Some`
    /// (belt-and-suspenders against a future re-enable being added
    /// only on one side of the lifecycle).
    #[cfg(windows)]
    prior_console_in_mode: Option<u32>,
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

/// Wraps the real stdout writer with an optional mirror file. When
/// `ATOMCODE_RENDER_DUMP=/path` is set at startup, every byte the
/// renderer writes to stdout is also appended to that file. Used to
/// diagnose xterm.js / shell-integration disagreements where the
/// renderer-model thinks one thing but the on-screen result is
/// different — having the exact byte stream lets us replay through
/// any terminal emulator out-of-band. No-op overhead when the env
/// var is unset (the `None` branch is a single conditional).
pub struct StdoutTap {
    inner: BufWriter<Stdout>,
    mirror: Option<File>,
}

impl Write for StdoutTap {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(ref mut f) = self.mirror {
            let _ = f.write_all(buf);
        }
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(ref mut f) = self.mirror {
            let _ = f.flush();
        }
        self.inner.flush()
    }
}

impl RetainedRenderer<StdoutTap> {
    pub fn new(caps: TerminalCaps) -> Self {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        let mirror = std::env::var("ATOMCODE_RENDER_DUMP")
            .ok()
            .filter(|s| !s.is_empty())
            .and_then(|path| File::create(path).ok());
        let tap = StdoutTap {
            inner: BufWriter::new(std::io::stdout()),
            mirror,
        };
        Self::with_writer(tap, caps, w, h)
    }
}

impl<W: Write + Send> RetainedRenderer<W> {
    pub fn with_writer(mut out: W, caps: TerminalCaps, w: u16, h: u16) -> Self {
        // Clear scrollback buffer so previous terminal content (e.g. git log)
        // doesn't remain visible above the atomcode viewport and mix with
        // the atomcode session transcript. `\x1b[3J` only affects scrollback;
        // it does not touch the visible screen rows.
        //
        // Mouse capture (`\x1b[?1002h` button-event + `\x1b[?1006h` SGR
        // coords) is intentionally NOT enabled here. We defer mouse wheel,
        // cmd+drag selection, and cmd+C copy to the terminal's native
        // handling — matches Claude Code's UX model. Trade-off: atomcode's
        // reverse-video drag selection and arboard/OSC52 clipboard write
        // path are no longer reachable from interactive events. The
        // disable-on-shutdown (`?1002l`/`?1006l`) sequences below are
        // preserved as defensive hygiene against any other actor (a child
        // process that exited weirdly, a prior atomcode run that
        // panicked before Drop) having left capture on.
        let _ = out.write_all(b"\x1b[3J");
        let _ = out.flush();
        #[cfg(windows)]
        // Conhost mouse capture intentionally skipped — see comment
        // above re: deferring to terminal-native selection/wheel/copy.
        let prior_console_in_mode: Option<u32> = None;
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
            assistant_line_buf: String::new(),
            md_state: crate::markdown::MdState::new(),
            dirty: false,
            last_painted_footer_rows: 0,
            skip_body_scroll_count: 0,
            scrolled_off: 0,
            welcome_banner: None,
            welcome_line_count: 0,
            live_spinner_active: false,
            inflight_tool: None,
            inflight_tool_rows: 0,
            live_group: None,
            #[cfg(windows)]
            prior_console_in_mode,
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
            crate::tuix_trace!(
                "BPOP",
                "site=inflight_zero len_before={} n={} scrolled_off={}",
                self.body_lines.len(),
                remove,
                self.scrolled_off
            );
            self.body_lines.truncate(self.body_lines.len() - remove);
            self.inflight_tool_rows = 0;
            return;
        }

        let bottom = self.body_bottom_row();
        let inplace_ok = prev_rows > 0 && n == prev_rows && bottom >= n as u16;
        if inplace_ok {
            // In-place rewrite: the prior render's terminal rows are at
            // (bottom - n + 1 ..= bottom). Update model state by
            // swapping the trailing slice; then walk each terminal row
            // with a position + erase + write triple.
            let keep = self.body_lines.len().saturating_sub(prev_rows);
            crate::tuix_trace!(
                "BPOP",
                "site=inflight_inplace len_before={} n={} scrolled_off={}",
                self.body_lines.len(),
                prev_rows,
                self.scrolled_off
            );
            self.body_lines.truncate(keep);
            let first = bottom - n as u16 + 1;
            for row in &new_rows {
                self.body_lines.push(row.clone());
            }
            for (i, row) in new_rows.iter().enumerate() {
                let r = first + i as u16;
                let seq = format!("\x1b[{};1H\x1b[2K", r);
                let _ = self.out.write_all(seq.as_bytes());
                let bytes = serialize_row(row);
                let _ = self.out.write_all(&bytes);
            }
            // DO NOT erase the footer here. This used to emit
            // `\x1b[{bottom+1};1H\x1b[0J` (ED 0) to wipe everything below
            // the inflight strip — i.e. the whole input box — on EVERY
            // 100ms spinner tick while a tool runs (WebSearch, cargo,
            // long bash). The repaint is deferred to the next ≤5ms
            // `flush_deferred`, so the box was blanked-then-repainted at
            // ~10Hz: the "执行 websearch 时输入框跳动" report. Same
            // non-atomic erase→repaint defect as `emit_body_line_inner`,
            // here on the inflight path.
            //
            // The erase was redundant. Its stated purpose (clear stale
            // inflight chars that leak when the footer later grows around
            // the strip) is now handled entirely by the cell-diff:
            // `invalidate_rows_from` sentinels prev_cells from the strip
            // top to the screen bottom, and `Cell::sentinel` forces a
            // patch on EVERY cell including blanks (it is deliberately
            // NOT `Cell::blank` — see `screen.rs`). So when a later
            // footer-growth frame lays a blank over an old marker column,
            // sentinel-vs-blank still emits a clearing patch — no leak,
            // no physical ED 0 needed, and the box never blanks. The
            // `retained_inflight_ghost_clears_when_footer_grows_around_it`
            // test asserts the final visual, not the ED 0, and still
            // passes. (The comment that previously lived here described
            // pre-sentinel behaviour where invalidate blanked prev_cells
            // and blank-vs-blank was a no-patch — stale since the
            // sentinel switch.)
            let first_0idx = (first as usize).saturating_sub(1);
            self.screen.invalidate_rows_from(first_0idx);
            self.dirty = true;
        } else {
            // First render or row-count mismatch — fall back to scroll-push.
            // Drop any prior inflight rows from model state; push new rows
            // via the standard path so DECSTBM scrolling lands them at the
            // bottom of the body region.
            let remove = prev_rows.min(self.body_lines.len());
            crate::tuix_trace!(
                "BPOP",
                "site=inflight_fallback len_before={} n={} scrolled_off={}",
                self.body_lines.len(),
                remove,
                self.scrolled_off
            );
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
    /// right side. Produces the CC-style per-conversation badge. The
    /// bot_rule keeps using `build_rule_row` (no badge there).
    ///
    /// Budget:
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
            super::MenuKind::Skill => {
                // Bare `<name>  <desc>` — no command prefix. Selection arrow
                // only. Pad by display width so CJK names align (same logic
                // as SlashCommand).
                let name_width = unicode_width::UnicodeWidthStr::width(name);
                let pad = 12usize.saturating_sub(name_width);
                let padded = format!("{}{}", name, " ".repeat(pad));
                if selected {
                    format!("▸ {}  {}", padded, desc)
                } else {
                    format!("  {}  {}", padded, desc)
                }
            }
            super::MenuKind::TwoColumn { row_prefix, selected_marker } => {
                // Name left-aligned, desc right-aligned. Rows fill the
                // full screen width so pad_row_to_width adds no trailing
                // spaces.  Optional selected_marker (show marker + space
                // on selected row, same-width space pad on others) and
                // row_prefix (prepended before name).
                let full_w = rule_width + PAD_COL * 2;
                let marker_w = unicode_width::UnicodeWidthStr::width(selected_marker);
                let indicator = if selected {
                    format!("{} ", selected_marker)
                } else {
                    " ".repeat(marker_w + 1) // marker + trailing space
                };
                let name = format!("{}{}", row_prefix, name);
                let indicator_w = marker_w + 1;
                let name_w = unicode_width::UnicodeWidthStr::width(name.as_str());
                if desc.is_empty() {
                    let rest = full_w.saturating_sub(indicator_w + name_w);
                    format!("{}{}{}", indicator, name, " ".repeat(rest))
                } else {
                    let sep: usize = 2;
                    // indicator_w + name + sep + desc must fit in full_w
                    let desc_w = unicode_width::UnicodeWidthStr::width(desc);
                    let avail = full_w.saturating_sub(indicator_w + sep);
                    if name_w + desc_w <= avail {
                        let pad = avail.saturating_sub(name_w + desc_w);
                        format!("{}{}{}{}", indicator, name, " ".repeat(pad + sep), desc)
                    } else {
                        let max_name = avail.saturating_sub(desc_w);
                        let truncated = crate::width::truncate_to_width(&name, max_name);
                        let truncated_w = unicode_width::UnicodeWidthStr::width(truncated.as_str());
                        let rest = full_w.saturating_sub(indicator_w + truncated_w + sep + desc_w);
                        format!("{}{}{}{}{}", indicator, truncated, " ".repeat(sep), desc, " ".repeat(rest))
                    }
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

        // Bypass indicator — right-aligned warning badge when
        // --dangerously-skip-permissions / -y is active. Rendered in
        // red (Role::Error) after the hint on the right side so
        // it does not displace the PLAN mode indicator on the left.
        let bypass_badge: Option<String> = status
            .bypass_indicator
            .as_ref()
            .map(|s| scrub_controls(s));
        let bypass_badge_w = bypass_badge
            .as_ref()
            .map(|s| crate::width::display_width(s) + 1) // +1 for the leading space separator
            .unwrap_or(0);

        // Hint right-alignment math must reserve space for the mode badge
        // and bypass badge so they never collide with the right-aligned
        // hint when the status row is wide.
        let max = rule_width.max(1);
        let right_reserved = mode_badge_w + bypass_badge_w;
        let left_max = max.saturating_sub(right_reserved);

        // Pre-truncate the cwd so that model + ctx_usage still get space
        // on narrow terminals.  Budget for cwd: subtract model width and
        // the " · " separator widths from left_max.  If the cwd alone
        // would eat the entire row, `truncate_path` replaces leading
        // segments with ".../" and keeps only the last segment.
        let model_str = if !status.model.is_empty() {
            let mut s = scrub_controls(&status.model);
            if let Some(ref effort) = status.reasoning_effort {
                use std::fmt::Write;
                let _ = write!(s, " [{}]", effort);
            }
            s
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

        // Helper: emit the bypass badge at the right edge of the row.
        // Rendered in red (Role::Error) to draw the eye — the user
        // must always see when tool calls are auto-approved.
        let bypass_style = self.style_for(Role::Error);
        let push_bypass = |row: &mut Vec<Cell>| {
            if let Some(badge) = &bypass_badge {
                push_str_cells(row, " ", &pad);
                push_str_cells(row, badge, &bypass_style);
            }
        };

        if let Some((raw_hint, severity)) = status.hint.as_ref() {
            let hint = scrub_controls(raw_hint);
            let hint_w = crate::width::display_width(&hint);
            let hint_style = match severity {
                crate::render::HintSeverity::Warning => error,
                crate::render::HintSeverity::Info => secondary.clone(),
            };
            let right_w = hint_w + bypass_badge_w;
            if right_w + 1 < left_max {
                let left_budget = left_max - right_w - 1;
                let left_truncated = crate::width::truncate_to_width(&left, left_budget);
                let left_w = crate::width::display_width(&left_truncated);
                let pad_w = max - right_reserved - left_w - hint_w;
                push_badge(&mut row);
                push_str_cells(&mut row, &left_truncated, &secondary);
                push_str_cells(&mut row, &" ".repeat(pad_w), &pad);
                push_str_cells(&mut row, &hint, &hint_style);
                push_bypass(&mut row);
            } else {
                let truncated = crate::width::truncate_to_width(&left, left_max);
                push_badge(&mut row);
                push_str_cells(&mut row, &truncated, &secondary);
                push_bypass(&mut row);
            }
        } else {
            let truncated = crate::width::truncate_to_width(&left, left_max);
            push_badge(&mut row);
            push_str_cells(&mut row, &truncated, &secondary);
            push_bypass(&mut row);
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
    ///   rows 3+N..3+N+M: menu items (M = up to half screen height)
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

        // Paginate menu using the kind-specific cap.
        let max_menu = self
            .menu
            .as_ref()
            .map(|m| m.kind.max_visible_rows(h, m.items.len()))
            .unwrap_or(4);
        let (menu_items, selected_in_view) = if let Some(m) = self.menu.as_ref() {
            let len = m.items.len();
            if len == 0 {
                (Vec::<(String, String)>::new(), None)
            } else {
                let offset = if len <= max_menu {
                    0
                } else if m.selected < max_menu {
                    0
                } else {
                    (m.selected + 1)
                        .saturating_sub(max_menu)
                        .min(len.saturating_sub(max_menu))
                };
                let end = (offset + max_menu).min(len);
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
        let menu_rows = menu_items.len().min(max_menu);
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
        // Append-only: footer sits directly below the last body row,
        // not pinned to the screen bottom. The VISIBLE body count is
        // `body_lines.len() - scrolled_off` (rows before `scrolled_off`
        // already promoted to native scrollback). When body+footer
        // would exceed screen height, the cap kicks in and the footer
        // pins to `h - total_rows` — the terminal's native scrollback
        // absorbs any further body growth via the LF in
        // `emit_body_line_inner`.
        let visible_body_len = self.body_lines.len().saturating_sub(self.scrolled_off);
        let body_rows_on_screen = visible_body_len.min(h.saturating_sub(total_rows));
        let footer_top = body_rows_on_screen;

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
        let h = self.screen.height() as usize;
        let menu_rows = self
            .menu
            .as_ref()
            .map(|m| m.kind.max_visible_rows(h, m.items.len()))
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

    /// Single-entry-point for painting a full frame. Append-only
    /// model: body is also painted into screen cells (in addition to
    /// being emitted via `\n` for scrollback), so the cell-diff
    /// stays consistent when the footer shifts row position as body
    /// grows. Without this, the cell-diff would emit blanks at the
    /// rows where prior footer cells lived — wiping the body row
    /// we just wrote there.
    fn paint_frame(&mut self) {
        self.paint_body_into_cells();
        self.paint_footer();
    }

    /// Append-only model: copy the body_lines tail into screen.cells
    /// at rows `[0, body_rows_on_screen)`. Width-clipped to the screen
    /// width. Mirrors the geometry math in `paint_footer` so the
    /// body+footer together exactly span `[0, body_rows + total_rows)`.
    fn paint_body_into_cells(&mut self) {
        let w = self.screen.width() as usize;
        let h = self.screen.height() as usize;
        if h == 0 || w == 0 || self.body_lines.is_empty() {
            return;
        }
        let footer_rows = self.current_footer_rows();
        let body_height = h.saturating_sub(footer_rows);
        if body_height == 0 {
            return;
        }
        let total = self.body_lines.len();
        // Visible window starts no earlier than `scrolled_off` —
        // anything before that index already lives in native
        // scrollback and must not be re-painted into the viewport,
        // otherwise the next overflow LF promotes it a second time
        // (see `scrolled_off` doc).
        let start = total
            .saturating_sub(body_height)
            .max(self.scrolled_off);
        if start >= total {
            return;
        }
        let body_width = self.screen.width() as usize;
        // Clone the slice before drawing — `screen.draw_row` takes
        // &mut self.screen and the iteration would otherwise double-
        // borrow.
        let rows: Vec<Vec<Cell>> = self.body_lines[start..].to_vec();
        for (i, row) in rows.iter().enumerate() {
            let clipped = clip_cells_to_width(row, body_width);
            self.screen.draw_row(i, 0, &clipped);
        }
        // Clear any rows between the last painted body row and the body
        // area boundary (body_height). When the footer shrinks (e.g. slash
        // menu closes), body_height grows, but the number of body_lines
        // may not fill the newly available space. Rows that were
        // previously occupied by the taller footer (which contained CJK
        // text from skill descriptions in the sub-mode menu) would retain
        // stale content in `self.cells` — the next `render_diff` would
        // compare that stale content against `prev_cells` (which has the
        // same stale content from the older frame) and find no diff,
        // leaving ghost CJK characters on screen.
        let painted = rows.len();
        if painted < body_height {
            let blank_row = vec![Cell::blank(); w.min(body_width)];
            for r in painted..body_height {
                self.screen.draw_row(r, 0, &blank_row);
            }
        }
    }

    /// 1-indexed row where the LAST EXISTING body row sits on
    /// screen. `0` means "no body rows on screen yet" — callers
    /// that operate on the existing tail (live spinner update,
    /// commit_inflight_tool erase, etc.) should treat 0 as
    /// "nothing to do".
    ///
    /// Append-only model: body grows downward from row 0 until it
    /// hits the viewport cap (`h - footer_rows`); past that, the
    /// terminal's native scrollback absorbs new rows so the last
    /// visible row always sits at `min(visible_body_len, h - footer_rows)`
    /// where `visible_body_len = body_lines.len() - scrolled_off`.
    fn body_bottom_row(&self) -> u16 {
        let h = self.screen.height() as usize;
        let footer_rows = self.current_footer_rows();
        let cap = h.saturating_sub(footer_rows);
        let visible_len = self.body_lines.len().saturating_sub(self.scrolled_off);
        visible_len.min(cap) as u16
    }

    /// 1-indexed row where the NEXT body emit would land. When the
    /// visible body still has room, this is `visible_body_len + 1`.
    /// Once the visible body fills the viewport, this saturates at the
    /// cap so the LF in `emit_body_line_inner` triggers terminal-native
    /// scrolling (the oldest visible row promotes to scrollback).
    fn next_body_emit_row(&self) -> u16 {
        let h = self.screen.height() as usize;
        let footer_rows = self.current_footer_rows();
        let cap = h.saturating_sub(footer_rows);
        let visible_len = self.body_lines.len().saturating_sub(self.scrolled_off);
        (visible_len + 1).min(cap) as u16
    }

    /// Append-only model: emit one body row via the
    /// "CUP-to-footer-top → erase-to-end-of-screen → write-row + LF"
    /// cycle. The cursor before this call is somewhere inside the
    /// footer (parked there by the last `paint_footer` →
    /// `render_diff`), so we use ABSOLUTE CUP — relative moves would
    /// land at the wrong row.
    ///
    /// The `bottom` parameter is retained for source compatibility
    /// with callers (`push_body_row`, `resume_from_external` body
    /// re-emit) but unused in the new model — position is computed
    /// from `body_lines.len()` and the screen geometry.
    ///
    /// `skip_body_scroll_count` is consumed (decremented) so callers
    /// that set it stay backwards-compatible, but the value no
    /// longer changes behaviour: in append-only mode, popping rows
    /// from `body_lines` (the trigger that arms `skip_body_scroll_count`
    /// in `pop_approval_prompt` / `commit_inflight_tool`) naturally
    /// shrinks the body region, and subsequent pushes naturally
    /// fill the freed slots via the next `paint_frame` cell-diff.
    /// The field will be deleted in a follow-up cleanup commit.
    fn emit_body_line_inner(&mut self, row: &[Cell], _bottom: u16) {
        if self.skip_body_scroll_count > 0 {
            self.skip_body_scroll_count -= 1;
        }
        let h = self.screen.height() as usize;
        let footer_rows = self.current_footer_rows();
        if h == 0 {
            return;
        }
        let cap = h.saturating_sub(footer_rows);
        if cap == 0 {
            // Footer fills the entire viewport — no room for body.
            return;
        }
        // Live scrollback feed: when this push would put body_lines.len()
        // above the visible cap, the oldest currently-visible row must
        // leave the viewport. Without help, the cell-diff just overwrites
        // it in place — the row vanishes without ever entering the host
        // terminal's native scrollback, so `cmd+↑` / mouse-wheel during
        // the session show nothing above the atomcode frame.
        //
        // Fix: position the cursor at the absolute last screen row and
        // emit LF. Terminals interpret LF at the bottom row as "scroll
        // the entire visible area up by 1, the row that was at the top
        // enters scrollback, the bottom row becomes blank". The cursor
        // lands at (h, 1) afterwards.
        //
        // The internal cells / prev_cells cache must mirror this
        // shift, otherwise the next paint_frame's cell-diff treats the
        // shift as "the whole frame moved" and emits a redundant
        // full-frame redraw. `screen.shift_prev_up(1)` performs the
        // same logical rotate on prev_cells so the diff stays small
        // (only the slot where the new body row lands plus the freshly
        // re-laid-out footer differ).
        //
        // `visible_len >= cap` is the entry condition because
        // `push_body_row` calls `emit_body_line_inner` BEFORE pushing
        // the new row. With visible_len == cap-1, the push lands at
        // visible-index cap-1 (still inside the visible region). With
        // visible_len == cap, the push would land at visible-index cap
        // (just past the visible region) — at that exact moment the
        // oldest visible row (`body_lines[scrolled_off]`) needs to
        // scroll out.
        //
        // `visible_len = body_lines.len() - scrolled_off` (NOT raw
        // `body_lines.len()`): rows at indices `< scrolled_off` already
        // live in native scrollback from a prior overflow LF. Using raw
        // len conflates them with visible rows and double-promotes the
        // front of `body_lines` after a tail pop (spinner clear,
        // approval pop, ...) — exact mechanism of the user-reported
        // "duplicate rows in scrollback" bug. See the `scrolled_off`
        // field doc for the full write-up.
        let mut visible_len = self.body_lines.len().saturating_sub(self.scrolled_off);
        let overflow_initial = visible_len >= cap;
        crate::tuix_trace!(
            "BEMIT",
            "len={} scrolled_off={} cap={} visible_len={} overflow={}",
            self.body_lines.len(),
            self.scrolled_off,
            cap,
            visible_len,
            overflow_initial
        );
        // Catch-up loop, not a single `if`: when `current_footer_rows()`
        // GROWS between two pushes (slash menu opens, multi-line input
        // wrap, attachment chip appears), `cap` shrinks but `visible_len`
        // doesn't — the prior bottom body rows are now logically under
        // the new footer strip yet still counted as visible. A single LF
        // (the original behaviour) only promotes ONE row, leaving
        // `visible_len = cap + N - 1` after the push. Net effect: the
        // direct write below lands at `target_1idx = visible_len + 1`,
        // which is BEYOND `cap` and stomps the footer/prompt cells.
        // Reproducer per BPUSH/BEMIT trace: at slash-menu open + Enter,
        // footer grows 4→7, cap shrinks 64→61, every /whoami push
        // emitted at `target_1idx=64` overlapping rows 62/63/64 of the
        // footer — visible as the screenshot's `❯ /whoami@csdn.net`
        // (the `cuizk@csdn.net` body row leaked into the user-echo
        // row's cells). LFing until `visible_len < cap` promotes the
        // exact excess to scrollback in one shot.
        while visible_len >= cap {
            let scroll_seq = format!("\x1b[{};1H\n", h);
            let _ = self.out.write_all(scroll_seq.as_bytes());
            self.screen.shift_prev_up(1);
            // The just-LFed row (front of the visible window) now lives
            // in native scrollback. Advance the marker so subsequent
            // pushes don't treat it as visible — and don't re-promote
            // it on the next overflow LF after an intervening tail pop.
            self.scrolled_off = self.scrolled_off.saturating_add(1);
            visible_len -= 1;
        }
        // 1-indexed row where the NEW body line should land on the
        // physical terminal. After the catch-up loop above
        // `visible_len < cap` always holds, so `visible_len + 1 ≤ cap`
        // is the next-empty body slot in both cases (loop ran ⇒
        // post-loop vl = cap-1, +1 = cap, matches the pre-refactor
        // overflow branch; loop didn't run ⇒ vl unchanged, +1 = the
        // natural append slot just under the existing tail). One
        // formula, no branch.
        //
        // The pre-refactor `if overflow { cap }` arm was load-bearing
        // against a regression where writing at `cap + 1` left a
        // ghost glyph below the body — the unified formula keeps that
        // fix intact. See `retained_overflow_does_not_duplicate_last_body_row`.
        let target_1idx = (visible_len + 1) as u16;
        // CUP to target → EL (`\x1b[K`, erase ONLY the target line),
        // NOT ED 0 (`\x1b[0J`, which also erases every footer row below
        // it). The footer is fully owned by the synchronized
        // `render_diff` path: `invalidate_rows_from(footer_top_0idx)`
        // below sentinels every row from here to the screen bottom, so
        // the next `flush_deferred` repaints the whole footer region
        // inside its DECSET 2026 envelope regardless of physical
        // pre-state — the ED 0 erase was REDUNDANT with that repaint.
        // Its only observable effect was a blank-input-box frame between
        // this eager direct write and the ≤5ms deferred repaint: during
        // streaming (a body line every few ms) the box strobes, very
        // visible on low-latency GPU terminals (the wezterm "输入框一直
        // 在闪动" report). Erasing just the target line keeps the new
        // body row's slot clean (no ghost tail) while leaving the footer
        // pixels untouched until the diff atomically repaints them. Same
        // lesson as `pop_approval_prompt` (per-row EL, never ED, so the
        // input box never vanishes). Pre-format into one buffer so the
        // write hits stdout as one call — the chunk-counting test
        // harness asserts on chunk boundaries.
        let seq = format!("\x1b[{};1H\x1b[K", target_1idx);
        let _ = self.out.write_all(seq.as_bytes());
        // Write the body row at target, then LF. The LF advances cursor
        // within the screen (no scroll) because target is always
        // strictly less than `h` (cap < h since footer_rows >= 1).
        let bytes = serialize_row(row);
        let _ = self.out.write_all(&bytes);
        let _ = self.out.write_all(b"\n");
        // ED 0 above blanked the physical terminal from `target_1idx`
        // down — but `screen.prev_cells` still holds whatever was there
        // last frame. Without resyncing, the next `render_diff` may
        // suppress a patch for a row whose newly-laid-out cells happen
        // to be byte-equal to the now-stale prev cells (the classic
        // case: the new footer's top_rule lining up with the old
        // footer's bot_rule — both are full rows of `─` in identical
        // style, so the diff sees "no change" and emits nothing, but
        // the physical terminal is blank there because the ED 0 wiped
        // it). Resync prev_cells to mirror the ED so the diff sees the
        // real delta and emits the necessary patches.
        let footer_top_0idx = target_1idx.saturating_sub(1) as usize;
        self.screen.invalidate_rows_from(footer_top_0idx);
        // Mark dirty so the next 5ms `paint_frame` tick redraws
        // the footer below the new body row via the cell-diff path.
        self.dirty = true;
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
        // After pop, the spinner row's screen slot is `next_body_emit_row`
        // (1-indexed) — that's where the spinner was and where the next
        // body emit will land. EL it now for immediate visual feedback;
        // the next `paint_frame` cell-diff also redraws this region but
        // the explicit erase removes any flash between pop and next tick.
        crate::tuix_trace!(
            "BPOP",
            "site=spinner len_before={} n=1 scrolled_off={}",
            self.body_lines.len(),
            self.scrolled_off
        );
        self.body_lines.pop();
        let target = self.next_body_emit_row();
        if target > 0 {
            let seq = format!("\x1b[{};1H\x1b[K", target);
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
        // Diagnostic trace for the user-reported "duplicate rows in
        // scrollback" bug — every push goes through here, so a single
        // log point captures the full sequence. Enable via
        // ATOMCODE_TUIX_LOG=/path. Snippet is the first ~40 chars of
        // the row's text content so duplicates are visually distinct
        // in the log.
        if crate::trace::enabled() {
            let snippet: String = row
                .iter()
                .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                .collect::<String>()
                .chars()
                .take(40)
                .collect();
            crate::tuix_trace!(
                "BPUSH",
                "len={} scrolled_off={} content={:?}",
                self.body_lines.len(),
                self.scrolled_off,
                snippet
            );
        }
        // Any external body push freezes an active live-group: the
        // group's child rows are no longer guaranteed to sit at the
        // bottom (they may have scrolled into native scrollback the
        // moment this push commits a `\n`). Future ToolGroupChildUpdate
        // events fall back to no-op rather than CUP-rewriting some
        // unrelated row that took the group child's screen position.
        self.live_group = None;
        if self.clear_live_spinner() {
            // `clear_live_spinner` already popped the spinner row from
            // `body_lines`. In append-only mode the next emit naturally
            // lands in the freed slot (footer_top shifted up by 1, the
            // new row pushes it back down). `skip_body_scroll_count`
            // remains armed for source-compat with callers that consult
            // it; its decrement path is now a no-op (kept to avoid a
            // bigger refactor in this combined commit).
            self.skip_body_scroll_count = self.skip_body_scroll_count.saturating_add(1);
        }
        // Append-only: `next_body_emit_row` checks the cap; emit is
        // a no-op when the footer occupies the whole viewport.
        // Unlike the old DECSTBM model we always emit on the first
        // push (no `bottom > 0` short-circuit) because emit_body_line
        // computes its own target row from `body_lines.len()`.
        if self.next_body_emit_row() > 0 {
            self.emit_body_line_inner(&row, 0);
        }
        self.body_lines.push(row);
        if self.body_lines.len() > MAX_SCROLLBACK_ROWS {
            let drain = self.body_lines.len() - MAX_SCROLLBACK_ROWS;
            self.body_lines.drain(0..drain);
            self.message_marks.retain(|m| m.line_idx >= drain);
            for m in self.message_marks.iter_mut() {
                m.line_idx -= drain;
            }
            // `scrolled_off` indexes the same vector — when we drop
            // front entries, slide it down by the same amount (saturating
            // because the drained rows were all `< scrolled_off` anyway:
            // we only ever drain rows that have already been promoted
            // to native scrollback once `body_lines` exceeds
            // MAX_SCROLLBACK_ROWS ≫ visible cap).
            self.scrolled_off = self.scrolled_off.saturating_sub(drain);
            // welcome_line_count is also a front-anchored index; keep
            // it consistent or `reflow_welcome_prefix.splice(0..wlc)`
            // would later splice over non-welcome rows.
            self.welcome_line_count = self.welcome_line_count.saturating_sub(drain);
        }
    }

    /// Record the start of a new logical message in `message_marks`.
    /// The mark's `line_idx` is set to the CURRENT length of `body_lines`
    /// (i.e. the index the NEXT `push_body_row` will occupy).
    /// Called before any push in the render arm that starts a new message.
    fn mark_message(&mut self, kind: crate::render::MarkKind) {
        let line_idx = self.body_lines.len();
        self.message_marks.push(crate::render::MessageMark {
            line_idx,
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
            // Update body_lines in place; the next `flush_deferred`
            // (≤5ms) lets the cell-diff path compute and emit only
            // the cells that actually changed between this tick and
            // the last (typically just the elapsed-seconds digits).
            //
            // Earlier this branch did its own direct write:
            //   `\x1b[{bottom};1H\x1b[K` + `serialize_row(&row_cells)`
            // The `\x1b[K` cleared the row visible before the serialize
            // re-painted it. Inside `render_diff`'s DECSET 2026
            // synchronized-output envelope that would be fine, but the
            // direct write bypassed the envelope entirely — so on hosts
            // that ignore BSU/ESU (pwsh7 on native Win10 conhost) the
            // user saw a per-tick "row blanks then refills left→right"
            // shake, with the leading icon stable (first byte after EL)
            // and the trailing chars wobbling as they trickled into the
            // terminal's cell buffer. Reported as 「字在左右抖动，图标
            // 还好」on the Pondering spinner. Letting the cell-diff
            // path do the work keeps the update inside the BSU envelope
            // (where supported) and produces minimal patches (where not),
            // which is the smallest visible change a host can render.
            if let Some(last) = self.body_lines.last_mut() {
                *last = row_cells;
            }
            self.dirty = true;
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
            // CRITICAL: capture the bottom row BEFORE truncating. With
            // the top-anchored body model, `body_bottom_row()` returns
            // `min(body_lines.len(), cap)` — calling it AFTER truncate
            // would point at the LAST `remove` rows of REMAINING body
            // content (the rows just above the inflight strip), which
            // erases real markers instead of the spinner rows we meant
            // to clear. The pre-truncate bottom is the 1-idx terminal
            // row of the LAST inflight row, exactly what we want.
            let bottom_before_truncate = self.body_bottom_row();
            crate::tuix_trace!(
                "BPOP",
                "site=commit_inflight len_before={} n={} scrolled_off={}",
                self.body_lines.len(),
                remove,
                self.scrolled_off
            );
            self.body_lines.truncate(self.body_lines.len() - remove);
            self.inflight_tool_rows = 0;
            if bottom_before_truncate > 0 && remove > 0 {
                // Erase ALL terminal rows previously occupied by the
                // inflight spinner (may be >1 when the command was long
                // enough to wrap). Without this, the old `⠙ Bash(...)`
                // row lingers on-screen above the freshly committed
                // `● Bash(...)` row, producing a visual duplicate.
                let start_row = bottom_before_truncate
                    .saturating_sub(remove as u16 - 1)
                    .max(1);
                let mut seq =
                    String::with_capacity((bottom_before_truncate - start_row + 1) as usize * 8);
                use std::fmt::Write as _;
                for row in start_row..=bottom_before_truncate {
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
    /// scrollback before we wipe the viewport on exit. Append-only
    /// model: body lives at the top of the viewport, so we repaint
    /// the tail at rows [1..=n], position the cursor at the bottom
    /// row of the terminal, and emit N LFs — each LF pushes the
    /// top row of the visible viewport into scrollback. After this
    /// runs `shutdown` proceeds to wipe the (now mostly blank)
    /// viewport, so it's fine that the footer also scrolls off.
    fn promote_visible_body_to_scrollback(&mut self) {
        let bottom = self.body_bottom_row() as usize;
        if bottom == 0 || self.body_lines.is_empty() {
            return;
        }

        let screen_w = self.screen.width() as usize;
        let screen_h = self.screen.height() as usize;
        let n = self.body_lines.len().min(bottom);
        if n == 0 {
            return;
        }
        let start = self.body_lines.len() - n;
        let rows: Vec<Vec<Cell>> = self.body_lines[start..]
            .iter()
            .map(|row| clip_cells_to_width(row, screen_w))
            .collect();

        // Repaint the tail at the top of the viewport (rows 1..=n).
        for (i, row) in rows.iter().enumerate() {
            let seq = format!("\x1b[{};1H\x1b[K", i + 1);
            let _ = self.out.write_all(seq.as_bytes());
            let bytes = serialize_row(row);
            let _ = self.out.write_all(&bytes);
        }
        // Park the cursor at the very bottom row of the terminal,
        // then emit N LFs. Each LF at the screen's last row triggers
        // a full-viewport scroll: the topmost row enters scrollback.
        // After N LFs all the rows we just painted have promoted.
        let park = screen_h.max(1) as u16;
        let seq = format!("\x1b[{};1H", park);
        let _ = self.out.write_all(seq.as_bytes());
        for _ in 0..n {
            let _ = self.out.write_all(b"\n");
        }
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
        let mut bytes = self.screen.render_diff();
        if let Some((r, c)) = self.screen.peek_cursor() {
            let cup = format!("\x1b[{};{}H", r, c);
            bytes.extend_from_slice(cup.as_bytes());
        }
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
        // Hint row(s): input prompt + /provider + /login.
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
        let webui_cmd = t(Msg::IdleHintWebui);
        let webui_suffix = t(Msg::IdleHintWebuiSuffix);
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
            "   ",
            webui_cmd.as_ref(),
            "  ",
            webui_suffix.as_ref(),
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
                    (&codingplan_cmd, accent_bold.clone()),
                    ("  ", hint_text.clone()),
                    (&codingplan_suffix, hint_text.clone()),
                    ("   ", hint_text.clone()),
                    (&webui_cmd, accent_bold),
                    ("  ", hint_text.clone()),
                    (&webui_suffix, hint_text),
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
                    (&codingplan_cmd, accent_bold.clone()),
                    ("  ", hint_text.clone()),
                    (&codingplan_suffix, hint_text.clone()),
                ],
                content_w,
            ));
            rows.extend(self.build_wrapped_text_rows(
                &[
                    (&webui_cmd, accent_bold),
                    ("  ", hint_text.clone()),
                    (&webui_suffix, hint_text),
                ],
                content_w,
            ));
        }

        // Trailing blank so subsequent async events (MCP "已连接",
        // upgrade hints, etc.) don't butt up against the hint row.
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
                // its contrast against assistant text. Fix: keep fg at
                // terminal default and only layer SGR 2 on top.
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
                // Display reasoning in faint style with word wrapping.
                //
                // Why CellStyle.faint instead of embedding `\x1b[2m...\x1b[0m`
                // in the text: `push_body_text` calls `push_str_cells`, which
                // treats every visible char as a width-1 cell — including
                // the SGR escape bytes ESC, `[`, `2`, `m` if they're in the
                // string. Cells get pushed at indices 0..N matching the
                // string's char positions, but when serialized back to
                // stdout the terminal swallows the SGR sequence without
                // advancing the cursor, so the cell-index ⇄ terminal-column
                // invariant breaks by exactly 4 (= len("\x1b[2m")) columns.
                // Subsequent cell-diff patches CUP to the model-column,
                // which on screen is 4 cells to the right of the visual
                // position — producing the "characters scattered into
                // word boundaries" corruption (e.g. "Now let me start"
                // rendered as "Now let letsmerstart" because the second
                // flush's diff patches landed at offset 4, overwriting
                // the spaces with letters from the new text).
                // Trace `BPUSH content="  \u{1b}[2m step by step."` showed
                // the ESC bytes embedded in cells; this fix routes the
                // faint attribute through `CellStyle` instead, where
                // `serialize_row` emits SGR around the run without
                // claiming cell-column space for the bytes.
                let text = scrub_controls(&text);
                let style = CellStyle {
                    faint: true,
                    ..CellStyle::default()
                };
                self.push_body_text(&text, &style);
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
                // Mark the batch header as a ToolCall anchor — Alt+↑/↓
                // (message-jump) walks `message_marks`; without this
                // the whole "● Running N calls in parallel" header +
                // child rows would be invisible to jump navigation,
                // skipping the entire batch.
                self.mark_message(crate::render::MarkKind::ToolCall);
                self.last_mark_was_assistant = false;
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
                // Children sit under the bold header. On dark themes
                // `Role::Muted` is SGR 37 (near-white) — the same tier as the
                // bold header — so the batch reads flat with no hierarchy.
                // Render children FAINT on dark to dim them to gray, matching
                // light theme (DarkGrey children under a black bold header) and
                // the single tool-call `●` fix.
                let muted = if crate::highlight::theme::is_light_for_render() {
                    self.style_for(Role::Muted)
                } else {
                    self.style_faint(Role::Muted)
                };
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

                // Match the initial render: faint on dark for hierarchy.
                let muted = if crate::highlight::theme::is_light_for_render() {
                    self.style_for(Role::Muted)
                } else {
                    self.style_faint(Role::Muted)
                };
                let new_row = build_one_row(&new_text, &muted, self.screen.width());

                // Update in-memory.
                if let Some(slot) = self.body_lines.get_mut(row_idx) {
                    *slot = new_row.clone();
                }

                // Compute terminal row position. body_bottom_row is the
                // bottom of the visible body strip; the live-group
                // children sit just above it. body_lines maps to
                // terminal rows from `body_bottom - (len-1)` upwards.
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
                // Mark the batch summary as a ToolResult anchor —
                // counterpart to the ToolGroupRender header above, so
                // Alt+↑/↓ can land on the closing line of a parallel
                // batch instead of skipping past the whole group.
                self.mark_message(crate::render::MarkKind::ToolResult);
                self.last_mark_was_assistant = false;
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
                // Same dark-theme color hierarchy as the `└` result line:
                // on dark themes `Role::Muted` is SGR 37 (near-white), so the
                // `●` anchor reads as the same tier as the bold command name
                // beside it. Render it FAINT on dark to dim it to a gray,
                // matching light theme (where `●` is DarkGrey against the
                // black bold name). Light theme keeps the plain muted color.
                let bullet_style = if crate::highlight::theme::is_light_for_render() {
                    self.style_for(Role::Muted)
                } else {
                    self.style_faint(Role::Muted)
                };
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
                    &bullet_style,
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
                // Dark-theme color hierarchy: on dark themes `Role::Muted`
                // resolves to SGR 37 (near-white), visually indistinct from
                // the header's default-fg — so the `● ToolName` header and
                // its `└` result line read as the same tier. Render the
                // muted hint FAINT on dark so it dims to a gray, restoring
                // the two-tier look light theme gets for free from
                // `MUTED_LIGHT` (DarkGrey). Light theme keeps the plain
                // muted color: DarkGrey is already a readable gray, and
                // faint-on-DarkGrey would over-dim it.
                let muted_hint = if crate::highlight::theme::is_light_for_render() {
                    self.style_for(Role::Muted)
                } else {
                    self.style_faint(Role::Muted)
                };
                let summary_style = muted_hint.clone();
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
                // to the tool-call header above (● ToolName). Reuses the
                // theme-aware `muted_hint` (faint on dark) computed above so
                // the `└` glyph dims in lockstep with the summary text.
                let prefix_style = muted_hint;
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
                // cell pipeline renders the same colours that the
                // plain renderer streams to stdout.
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
                    crate::tuix_trace!(
                        "BPOP",
                        "site=image_blank len_before={} n=1 scrolled_off={}",
                        self.body_lines.len(),
                        self.scrolled_off
                    );
                    self.body_lines.pop();
                    let bottom = self.body_bottom_row();
                    if bottom > 0 {
                        let seq = format!("\x1b[{};1H\x1b[K", bottom);
                        let _ = self.out.write_all(seq.as_bytes());
                    }
                    self.skip_body_scroll_count =
                        self.skip_body_scroll_count.saturating_add(1);
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
        //
        // CRITICAL: capture `bottom` BEFORE the pop loop runs. In the
        // top-anchored body model, `body_bottom_row()` returns
        // `min(body_lines.len(), cap)` — calling it AFTER popping
        // would point at the LAST `popped_count` rows of REMAINING
        // body content (the rows just above the approval strip), so
        // the erase would wipe real markers instead of the formerly-
        // approval rows. Capture once, before mutating body_lines.
        let bottom_before_pop = self.body_bottom_row();
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
        crate::tuix_trace!(
            "BPOP",
            "site=approval len_after={} n={} scrolled_off={}",
            self.body_lines.len(),
            popped_count,
            self.scrolled_off
        );
        // Physically wipe the popped rows for instant visual feedback
        // on Y/A/N. The popped rows occupied terminal rows
        // `bottom_before_pop - popped_count + 1 ..= bottom_before_pop`
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
        if bottom_before_pop > 0 {
            // Erase the popped body rows (may span multiple terminal
            // lines). Use per-row \x1b[K instead of \x1b[J to avoid
            // erasing the footer rows below the body strip.
            // screen.prev_cells still holds the old footer content,
            // so without invalidation the next render_diff() would
            // see identical prev/current footer cells and skip the
            // repaint — leaving the footer permanently blank.
            // invalidate() below ensures the next flush_deferred()
            // emits a full repaint of every non-blank cell.
            let start_row = bottom_before_pop.saturating_sub(popped_count - 1).max(1);
            let mut seq = String::with_capacity((bottom_before_pop - start_row + 1) as usize * 8);
            use std::fmt::Write as _;
            for row in start_row..=bottom_before_pop {
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
        // Append-only top-anchored: body_lines[i] sits at terminal
        // row `i - (n - bottom) + 1` (1-indexed) where `bottom` is
        // the visible body row count. When body hasn't overflowed
        // (n <= bottom), every welcome line lives on-screen. When
        // body has overflowed, welcome lines whose offset falls
        // below the viewport top have scrolled past and aren't
        // reachable for in-place refresh.
        let mut seq: Vec<u8> = Vec::with_capacity(self.welcome_line_count * 64);
        seq.extend_from_slice(b"\x1b7");
        let mut wrote = false;
        let viewport_start = n.saturating_sub(bottom);
        for i in 0..self.welcome_line_count.min(n) {
            if i < viewport_start {
                // Welcome line has scrolled past the viewport top.
                continue;
            }
            let abs = (i - viewport_start) + 1;
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
            // Cells on those rows now hold the new content —
            // invalidate the diff cache so the next frame doesn't
            // decide the row is unchanged based on the stale
            // snapshot.
            self.screen.invalidate();
        }
        self.dirty = true;
    }

    fn shutdown(&mut self) {
        // Disable mouse capture (button-event + SGR coordinates) so the
        // terminal returns to default mouse behavior when atomcode exits.
        let _ = self.out.write_all(b"\x1b[?1006l\x1b[?1002l");
        #[cfg(windows)]
        if let Some(prior) = self.prior_console_in_mode.take() {
            crate::render::conhost::restore_conhost_console_in_mode(prior);
        }
        let _ = self.out.flush();
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
        // Pop Kitty keyboard enhancement flags pushed at startup.
        // Without this the terminal keeps sending CSI u sequences
        // (e.g. `9;5:3u`) for every keypress after we exit, and
        // the parent shell echoes them as literal gibberish.
        if self.caps.tty {
            let _ = execute!(self.out, PopKeyboardEnhancementFlags);
        }
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
        // unambiguous everywhere (same technique used by
        // `on_resize` and `clear_screen`).
        //
        // Release DECSTBM first so EL isn't constrained by any
        // scroll region a child process may have left set.
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
        self.scrolled_off = 0;
        self.welcome_line_count = 0;
        self.message_marks.clear();
        self.assistant_line_buf.clear();
        self.md_state.reset();
        self.last_painted_footer_rows = 0;
        let _ = self.out.flush();
        // Force cold-start CUP+EL on next paint to wipe pre-reset stdout writes.
        self.screen.invalidate();
    }

    fn clear_screen(&mut self) {
        // Same as reset for retained mode — Screen IS our model, so
        // wiping the terminal requires wiping the model too. The
        // old AnsiRenderer had a distinction because its cache was
        // a leaky abstraction; retained mode closes that hole.
        self.reset();
    }

    fn suspend_for_external(&mut self) {
        // Disable mouse capture so the external child process (OAuth browser,
        // shell prompt, etc.) runs with a clean terminal state. Disable order:
        // SGR first, then button-event. Mouse mode must be off before
        // raw_mode is disabled, so the child process sees the terminal with
        // mouse disabled.
        let _ = self.out.write_all(b"\x1b[?1006l\x1b[?1002l");
        #[cfg(windows)]
        if let Some(prior) = self.prior_console_in_mode.take() {
            crate::render::conhost::restore_conhost_console_in_mode(prior);
        }
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
        let _ = self.out.flush();
        // Re-emit body tail so the view matches `body_lines` again.
        // Append-only top-anchored: tail lives at rows [first_row, bottom];
        // per-row absolute CUP + EL + content avoids any LF (no
        // scrollback pollution) and mirrors the same machinery on_resize
        // uses.
        let bottom = self.body_bottom_row();
        if bottom > 0 {
            let tail: Vec<Vec<Cell>> = {
                let n = self.body_lines.len().min(bottom as usize);
                self.body_lines[self.body_lines.len() - n..]
                    .iter()
                    .cloned()
                    .collect()
            };
            let n = tail.len() as u16;
            let first_row = bottom.saturating_sub(n) + 1;
            for (i, row) in tail.iter().enumerate() {
                let seq = format!("\x1b[{};1H\x1b[K", first_row + i as u16);
                let _ = self.out.write_all(seq.as_bytes());
                let bytes = serialize_row(row);
                let _ = self.out.write_all(&bytes);
            }
        }
        let _ = self.out.flush();
        // Mouse capture intentionally NOT re-enabled on resume. We deferred
        // mouse wheel / cmd+drag selection / cmd+C copy to the terminal's
        // native handling at startup (see with_writer comment), so resume
        // must keep that contract — re-enabling here would suddenly steal
        // wheel events back from the terminal after the user returned from
        // an external subprocess. The matching disable in
        // suspend_for_external is still emitted so any subprocess that
        // somehow turned capture on during its run gets cleaned up here.
        #[cfg(windows)]
        {
            // Mirror the lib-level decision: leave conhost mode alone.
            self.prior_console_in_mode = None;
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
            let h = self.screen.height() as usize;
            let menu_rows = self
                .menu
                .as_ref()
                .map(|m| m.kind.max_visible_rows(h, m.items.len()))
                .unwrap_or(0);
            let middle_rows = footer_rows.saturating_sub(
                1 /* spinner */
                + 1 /* top rule */
                + 1 /* bot rule */
                + menu_rows
                + if has_status { 1 } else { 0 },
            );
            let buf_display_w = crate::width::display_width(&self.input_buf);
            self.paint_frame();
            let mut bytes = self.screen.render_diff();
            // Re-anchor cursor: `render_diff`'s trailing DECTCEM
            // (\x1b[?25h) may obscure the preceding CUP on terminals
            // like iTerm2 that treat the visibility toggle as a
            // cursor-position side-effect. Append one extra CUP so it
            // rides the same chunked write as the rest of the frame.
            if let Some((r, c)) = self.screen.peek_cursor() {
                let cup = format!("\x1b[{};{}H", r, c);
                bytes.extend_from_slice(cup.as_bytes());
            }
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

    fn scroll_body(&mut self, _delta: i32) {
        // No-op by design. After the append-only refactor body rows
        // live in the host terminal's native scrollback once they
        // scroll off the visible region, so vertical navigation is the
        // terminal's job.
        //
        // Mouse capture is now intentionally disabled at startup
        // (see `with_writer`), so crossterm never emits Event::Mouse
        // events on stdin and this method effectively never gets
        // called from interactive use. The terminal handles wheel
        // ticks directly — scrollback via plain wheel, native
        // selection via drag, copy via cmd+C — which is the whole
        // point of the trade-off.
        //
        // See regression test `retained_scroll_body_is_noop_to_preserve_native_scrollback`.
    }

    fn scroll_body_to_top(&mut self) {
        // No-op: terminal-native scrollback handles vertical navigation
        // after the append-only refactor.
    }

    fn scroll_body_to_bottom(&mut self) {
        // No-op: terminal-native scrollback handles vertical navigation
        // after the append-only refactor.
    }

    fn scroll_to_prev_message(&mut self) {
        // No-op: terminal-native scrollback handles vertical navigation
        // after the append-only refactor.
    }

    fn scroll_to_next_message(&mut self) {
        // No-op: terminal-native scrollback handles vertical navigation
        // after the append-only refactor.
    }

    fn scroll_to_prev_user_message(&mut self) {
        // No-op: terminal-native scrollback handles vertical navigation
        // after the append-only refactor.
    }

    fn scroll_to_next_user_message(&mut self) {
        // No-op: terminal-native scrollback handles vertical navigation
        // after the append-only refactor.
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
        // Diagnostic (opt-in via ATOMCODE_TUIX_LOG): trace each resize phase
        // BEFORE the corresponding console write, so a conhost fastfail during
        // a window drag still leaves the killing phase as the last RSZ line.
        // `legacy_conhost` here confirms whether the ED2-safe path is active.
        crate::tuix_trace!(
            "RSZ",
            "enter old={}x{} new={}x{} legacy_conhost={}",
            self.screen.width(),
            self.screen.height(),
            cols,
            rows,
            self.caps.legacy_conhost
        );
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
        if self.caps.legacy_conhost {
            // Classic Windows conhost (10.0.19041) fastfails (0xc0000409 —
            // "整个终端窗口直接消失") when it receives the per-row CUP+EL wipe
            // burst below while its console buffer is mid-resize (window drag).
            // A single ED2 (erase whole display) + home is ONE sequence conhost
            // handles cleanly, so it sidesteps the crash. We avoid ED2 on other
            // terminals (see note below) only because iTerm2 3.5+ ignores it
            // under some states — conhost honors it, so it's the safe choice
            // on this host specifically.
            let _ = self.out.write_all(b"\x1b[2J\x1b[H");
            crate::tuix_trace!("RSZ", "wipe=ED2 done");
        } else {
            let mut seq = String::with_capacity((rows as usize) * 8 + 8);
            for row in 1..=(rows as usize) {
                use std::fmt::Write;
                let _ = write!(seq, "\x1b[{};1H\x1b[K", row);
            }
            seq.push_str("\x1b[H");
            crate::tuix_trace!("RSZ", "wipe=perrow rows={} bytes={}", rows, seq.len());
            let _ = self.out.write_all(seq.as_bytes());
        }
        crate::tuix_trace!("RSZ", "wipe written");
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
            // Append-only: body lives at the top of the viewport, so
            // we draw the tail at absolute rows [1, n] without any
            // DECSTBM region. Per-row CUP + EL + content; never an
            // LF (which could nudge content into scrollback).
            let n = tail.len() as u16;
            let first_row = bottom.saturating_sub(n) + 1;
            crate::tuix_trace!("RSZ", "body begin rows={} first_row={}", n, first_row);
            for (i, row) in tail.iter().enumerate() {
                let seq = format!("\x1b[{};1H\x1b[K", first_row + i as u16);
                let _ = self.out.write_all(seq.as_bytes());
                let bytes = serialize_row(row);
                let _ = self.out.write_all(&bytes);
            }
        }
        crate::tuix_trace!("RSZ", "body done; paint begin");
        self.paint_frame();
        self.flush_frame();
        let _ = self.out.flush();
        crate::tuix_trace!("RSZ", "done");
        self.last_painted_footer_rows = self.current_footer_rows();
        self.dirty = false;
    }

}

impl<W: Write + Send> Drop for RetainedRenderer<W> {
    /// Belt-and-suspenders cleanup for the panic path. `shutdown()`
    /// already runs on graceful exit, but a panic anywhere above this
    /// layer (e.g. inside `paint_frame`, channel send failures, OOM in
    /// a downstream consumer) bypasses `shutdown()` and would otherwise
    /// leave the host shell with: SGR mouse reports landing as stdin
    /// garbage on every click, a hidden cursor (spinner DECTCEM off
    /// never restored), autowrap off, or a leftover DECSTBM scroll
    /// region. Without this Drop, retained leaves the terminal in a
    /// broken state on any panic after `with_writer` ran.
    ///
    /// Minimal cleanup only — no paint, no flush retries, no body
    /// promotion. All sequences are idempotent: `shutdown()` emits the
    /// same bytes earlier on the graceful path, and the terminal
    /// accepts the duplicates as no-ops.
    fn drop(&mut self) {
        // Mouse-mode disable + Kitty keyboard flag pop + cursor show
        // (DECTCEM) + autowrap on (DECAWM) + release any DECSTBM
        // scroll region. The latter three mirror `shutdown()`'s
        // force-restore so a panic mid-spinner doesn't leak
        // DECTCEM-off into the parent shell.
        // \x1b[>1u pops one level of Kitty keyboard enhancement
        // (same as crossterm's PopKeyboardEnhancementFlags).
        let _ = self
            .out
            .write_all(b"\x1b[?1006l\x1b[?1002l\x1b[>1u\x1b[?25h\x1b[?7h\x1b[r");
        #[cfg(windows)]
        if let Some(prior) = self.prior_console_in_mode.take() {
            crate::render::conhost::restore_conhost_console_in_mode(prior);
        }
        let _ = self.out.flush();
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

    #[test]
    fn ctx_usage_million_window_renders_as_m_not_thousand_k() {
        // 1m-context models would previously show `1000k`, mixing k/m in
        // the user's head and burning width on a leading zero parade.
        // Round million → bare `1m`; non-round million → one-decimal `1.5m`.
        assert_eq!(format_ctx_usage(1_400, 1_000_000), "1.4k/1m tok");
        assert_eq!(format_ctx_usage(50_000, 2_000_000), "50.0k/2m tok");
        assert_eq!(format_ctx_usage(50_000, 1_500_000), "50.0k/1.5m tok");
    }

    #[test]
    fn ctx_usage_used_above_one_million_uses_m_unit() {
        // Long-running sessions on a 1m window can park `used` above 1M;
        // keep one decimal so the counter still moves visibly turn-to-turn.
        assert_eq!(format_ctx_usage(1_200_000, 1_000_000), "1.2m/1m tok");
        assert_eq!(format_ctx_usage(2_500_000, 0), "2.5m tok");
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
            bypass_indicator: None,
            session_name: None,
            reasoning_effort: None,
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
            bypass_indicator: None,
            session_name: None,
            reasoning_effort: None,
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

    /// Bypass indicator (--dangerously-skip-permissions) renders on the
    /// RIGHT side of the status row, after the model · cwd run. It must
    /// NOT displace the left-aligned PLAN mode indicator.
    #[test]
    fn build_status_row_bypass_badge_on_right_side() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let status = StatusLine {
            model: "glm-5".into(),
            cwd: "~/proj".into(),
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: Some("PLAN".into()),
            bypass_indicator: Some("\u{26a0} BYPASS".into()),
            session_name: None,
            reasoning_effort: None,
        };
        let row = r.build_status_row(&status, 60);
        let visible: String = row.iter().map(|c| c.ch).collect();
        let trimmed = visible.trim_start();
        // PLAN badge still on the left.
        assert!(
            trimmed.starts_with("PLAN "),
            "PLAN badge must still appear first on the left; got: {:?}",
            visible
        );
        // BYPASS badge appears somewhere in the row (right side).
        assert!(
            visible.contains("BYPASS"),
            "BYPASS badge must appear in the row; got: {:?}",
            visible
        );
        // PLAN comes before BYPASS in the rendered row.
        let plan_pos = visible.find("PLAN").expect("PLAN must be present");
        let bypass_pos = visible.find("BYPASS").expect("BYPASS must be present");
        assert!(
            plan_pos < bypass_pos,
            "PLAN ({}) must precede BYPASS ({}); got: {:?}",
            plan_pos,
            bypass_pos,
            visible
        );
    }

    /// Bypass indicator without a mode indicator (Build + BYPASS) — the
    /// badge still appears on the right, and no PLAN badge leaks.
    #[test]
    fn build_status_row_bypass_without_mode_indicator() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let status = StatusLine {
            model: "glm-5".into(),
            cwd: "~/proj".into(),
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: None,
            bypass_indicator: Some("\u{26a0} BYPASS".into()),
            session_name: None,
            reasoning_effort: None,
        };
        let row = r.build_status_row(&status, 60);
        let visible: String = row.iter().map(|c| c.ch).collect();
        assert!(
            visible.contains("BYPASS"),
            "BYPASS badge must appear; got: {:?}",
            visible
        );
        assert!(
            !visible.contains("PLAN"),
            "no mode_indicator should produce no PLAN badge; got: {:?}",
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
    /// footer spinner. Budget: < 300 B/iteration.
    ///
    /// History: 200 → 250 when the retained renderer added an extra
    /// full-frame cost for the trailing StreamingBox re-paint.
    /// 250 → 300 when `invalidate_rows_from` switched from
    /// `Cell::blank` to `Cell::sentinel` so every cell in the
    /// invalidated region — including the trailing spaces inside the
    /// footer's input box and status row — re-patches every frame.
    /// That sentinel change kills the win10+pwsh7+zh_CN char-doubling
    /// class of bugs (see `Cell::sentinel` doc); ~14 B/iter is the
    /// shipping cost.
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
            avg_per_delta < 300,
            "retained streaming regressed: {} B/iter (budget < 300)",
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

    /// Regression: after a `/skills` menu containing CJK skill names/descs
    /// closes, no glyph fragments may linger on screen. The user saw stray
    /// `式` / `致` characters at column 0 below the input box after browsing
    /// the installed-skills list and then clearing the buffer.
    #[test]
    fn closing_cjk_skill_menu_leaves_no_residual_glyphs() {
        // Small screen so body + the menu-grown footer compete for rows and
        // the body must scroll when the menu opens (and back when it closes).
        let (mut r, buf) = new_capturing(80, 12);
        let status = status_basic();

        // Some ASCII body content (welcome-banner stand-in).
        for i in 0..6 {
            r.render(UiLine::User(format!("line-{i}")));
        }

        // A skills menu with CJK content, larger than the visible window so
        // it paginates (mirrors "scroll down through installed skills").
        let items: Vec<(String, String)> = (0..12)
            .map(|i| (format!("技能{i}"), "代码格式化导致的问题".to_string()))
            .collect();
        r.render(UiLine::InputPrompt {
            buf: "/skills ".into(),
            cursor_byte: "/skills ".len(),
            menu: Some(MenuPayload {
                items: items.clone(),
                selected: 8, // scrolled down past the first page
                kind: crate::render::MenuKind::Skill,
            }),
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Close the menu (user cleared the buffer).
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        let mut vterm = crate::test_term::VirtualTerminal::new(80, 12);
        drain_into_vterm(&buf, &mut vterm);

        for row in 0..12 {
            let text = vterm.row_text(row);
            assert!(
                !text.contains('式')
                    && !text.contains('致')
                    && !text.contains('技')
                    && !text.contains('能'),
                "residual menu glyph lingered on row {}: {:?}",
                row,
                text
            );
        }
    }

    /// Regression: user showed a 5-column CJK table with long cells
    /// overflowing past the terminal's right edge — `flush_aligned_table`
    /// was ignoring terminal width. This test verifies the full pipeline
    /// (streamed assistant text → `render_line_with_width` → body_lines)
    /// keeps every rendered body row within screen width.
    // Regression for the `/sync` blank-assistant-reply bug: a streamed
    // assistant reply with NO trailing newline (e.g. "在的！") only reaches
    // scrollback once an AssistantLineBreak flushes the partial buffer.
    // `flush_assistant_lines` commits only `\n`-terminated lines, so without
    // the line break the reply stays parked in `assistant_line_buf` forever.
    // In sync mode the PeerBusy(false) handler now emits that line break on
    // the peer's turn completion (the forwarder never sends TurnComplete).
    #[test]
    fn assistant_partial_line_commits_only_after_line_break() {
        let (mut r, _c) = new_counting(80, 24);
        r.render(UiLine::User("123".into()));
        r.render(UiLine::AssistantText("在的！".into())); // no trailing '\n'

        // Wide CJK glyphs occupy 2 cells, so cell-by-cell extraction inserts a
        // padding space after each — strip spaces before substring-matching.
        let body = |r: &RetainedRenderer<CountingSink>| -> String {
            r.body_lines
                .iter()
                .map(|row| row.iter().map(|c| c.ch).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
                .replace(' ', "")
        };
        // Buffered, not yet in scrollback (the exact state the bug got stuck in).
        assert!(
            !body(&r).contains("在的！"),
            "partial line must stay buffered until a line break; body:\n{}",
            body(&r)
        );
        // Turn completion (AssistantLineBreak) flushes it to scrollback.
        r.render(UiLine::AssistantLineBreak);
        assert!(
            body(&r).contains("在的！"),
            "assistant reply must be committed after AssistantLineBreak; body:\n{}",
            body(&r)
        );
    }

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

    /// Regression: user reports that during Ctrl+O verbose streaming with a
    /// slash menu open mid-flight, the row that PREVIOUSLY held the inflight
    /// tool icon leaks chars through the new menu/status content drawn on
    /// top of it.
    ///
    /// Root cause: `render_inflight_tool`'s in-place path uses `\x1b[2K`
    /// (EL2) to clear ONLY the rows it's about to write, and
    /// `invalidate_rows_from(first_0idx)` to blank `prev_cells` from the
    /// new first row downward. When the footer subsequently grows (menu
    /// opens → footer +4 rows → body shrinks by 4), the OLD inflight rows
    /// are now BELOW the new body_bottom — but the prior in-place write
    /// left their physical-terminal chars in place. `cell-diff` then
    /// compares the NEW frame (footer rows with leading blanks like
    /// `"  PLAN ..."`) against the BLANKED prev_cells and emits patches
    /// only for cells that are non-blank in the new frame. Blank-vs-blank
    /// columns get no patch — the terminal physical content (stale OLD
    /// inflight chars) survives and shows through wherever the new content
    /// has a space.
    ///
    /// Fix: at the end of `render_inflight_tool`'s in-place path, clear
    /// the physical terminal below the new inflight rows (`ED 0` at row
    /// bottom+1) so the next paint's blank cells overlay a known-blank
    /// physical row instead of stale chars.
    #[test]
    fn retained_inflight_ghost_clears_when_footer_grows_around_it() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // Baseline: empty input, no menu, no body.
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋".into(),
            label: "Pondering".into(),
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Fill body to JUST FILL the cap. Footer_rows=4 (top+mid+bot+status),
        // cap=20. After 20 body lines, body_lines.len()==20, visible_len==20.
        for i in 0..20 {
            r.render(UiLine::CommandOutput(format!("filler-{:02}\n", i)));
        }
        r.flush_deferred();

        // Now seed an inflight tool. The detail string carries a unique
        // ASCII marker so we can spot leakage after the menu opens. The
        // marker is positioned in `Bash(LEAKMARKER...)` so when the menu
        // row "  /status..." overlays it, the columns past col 8 (the
        // end of "/status") receive no patches (blank-in-new == blank-
        // in-blanked-prev) and the marker chars survive on the physical
        // terminal — visible in `row_text` via the vterm.
        r.render(UiLine::ToolCallInFlight {
            id: "call-leak".into(),
            name: "Bash".into(),
            detail: "LEAKMARKER_PLACEHOLDER_AAAA".into(),
        });
        // Drive an in-place tick so the icon updates via the in-place
        // path (prev_rows>0). This is what the user's session looks
        // like every 80ms while the tool runs.
        r.render(UiLine::Spinner {
            frame: "⠙".into(),
            label: "Running Bash".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Sanity: the marker should be at the body's last visible row
        // (1-indexed row 20 = vterm row index 19). If this assertion
        // fails, the test setup is wrong (geometry assumption broken)
        // — not the bug under test.
        let inflight_row_idx = 19;
        let pre_open = vterm.row_text(inflight_row_idx);
        assert!(
            pre_open.contains("LEAKMARKER"),
            "test setup broken: expected inflight row at vterm row {} \
             to carry the marker before menu opens; got: {:?}\n{}",
            inflight_row_idx + 1,
            pre_open,
            vterm.dump()
        );

        // OPEN THE SLASH MENU — footer_rows grows 4 → 8. body_height
        // shrinks 20 → 16. body_bottom moves up from row 20 to row 16.
        // The OLD inflight row 20 is now part of the FOOTER strip (the
        // first menu item). The new render_inflight_tool's in-place
        // write goes to row 16; row 20's stale physical chars are NOT
        // touched by render_inflight_tool, and the next paint's cell-
        // diff vs blanked prev_cells can't erase them at columns where
        // the new menu row has spaces.
        let items: Vec<(String, String)> = vec![
            ("status".into(), "Show status".into()),
            ("session".into(), "New session".into()),
            ("skills".into(), "Show skills".into()),
            ("settings".into(), "Show settings".into()),
        ];
        r.render(UiLine::StreamingBox {
            buf: "/s".into(),
            cursor_byte: 2,
            frame: "⠹".into(),
            label: "Running Bash".into(),
            menu: Some(MenuPayload {
                // Selected=1 → /session selected → first menu row
                // (/status) is NOT selected, so its col 0 is blank
                // (the bug-revealing case). A selected first row would
                // start with '▸' at col 0 and mask the leak there.
                items,
                selected: 1,
                kind: crate::render::MenuKind::SlashCommand,
            }),
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // The row that used to carry the inflight marker must now hold
        // ONLY the new menu row content. Any survival of "MARKER" or
        // "LEAK" chars proves the in-place direct-write didn't clean up
        // its old physical-terminal position when the footer grew under
        // it.
        let post_open = vterm.row_text(inflight_row_idx);
        assert!(
            !post_open.contains("MARKER"),
            "inflight chars leaked through to row {} after menu opened: {:?}\n{}",
            inflight_row_idx + 1,
            post_open,
            vterm.dump()
        );
        assert!(
            !post_open.contains("LEAK"),
            "inflight 'LEAK' substring survived menu opening at row {}: {:?}\n{}",
            inflight_row_idx + 1,
            post_open,
            vterm.dump()
        );
        // Positive check: the new content (menu row '/status') should be
        // visible at that row — guards against an over-broad fix that
        // wipes legitimate footer content along with the leak.
        assert!(
            post_open.contains("/status"),
            "menu row '/status' content missing at row {}: {:?}\n{}",
            inflight_row_idx + 1,
            post_open,
            vterm.dump()
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
        // Append-only layout: body is empty, footer hugs the top of
        // the screen. Middle row holding "originaltag" sits at row 1
        // (top_rule=0, middle=1, bot_rule=2, status=3).
        assert!(vterm.row_text(1).contains("originaltag"));

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

        // Append-only layout: footer follows body (empty here) from
        // the top. We don't pin bot_rule to H-2 anymore — find the
        // LAST full-width '─' row dynamically (top_rule is the same
        // glyph but earlier; bot_rule is the one followed by status).
        // Input box is flush-left/right (no PAD_COL) — every col
        // 0..w should be '─' on whichever row holds the bot_rule.
        let bot_rule_row = (0..24usize)
            .rev()
            .find(|&r| (0..40usize).all(|c| vterm.cell_at(r, c).ch == '─'))
            .unwrap_or_else(|| {
                panic!(
                    "no full-width rule row found; dump:\n{}",
                    vterm.dump()
                )
            });
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

        // Append-only layout: body empty, footer hugs the top.
        // Footer 4 rows: top_rule=0, middle=1, bot_rule=2, status=3.
        // "你是谁" in middle row (col 0-indexed, flush-left now):
        //   col 0 '❯', col 1 ' ',
        //   col 2 '你' (cols 2-3, right half blank), col 4 '是',
        //   col 6 '谁'.
        //   (caps_with_color has unicode_symbols=true so prompt_chevron() is "❯ ".)
        let middle_row = 1;
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

        // Append-only layout: body empty, footer hugs the top.
        // Footer with menu = top_rule + middle + bot_rule + 4 menu
        // + status = 8 rows. Layout from row 0:
        //   row 0: top rule
        //   row 1: middle ("❯ /")
        //   row 2: bot rule
        //   rows 3-6: menu rows (selected @ 3)
        //   row 7: status
        //
        // Inspect menu row 0 (row 3): reverse-video strip starting
        // from PAD_COL, with "▸" marker present.
        let menu0_row = 3;
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

        // Non-selected row (menu row 1 = screen row 4) must NOT be
        // reverse-video.
        let row1_text = vterm.row_text(4);
        assert!(
            row1_text.contains("/provider"),
            "menu row 1 missing: {:?}",
            row1_text
        );
        let provider_col = row1_text.find('/').unwrap();
        assert!(
            !vterm.cell_at(4, provider_col).reverse,
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

        // Append-only top-anchored: 7 welcome rows occupy rows 0..=6,
        // footer 4 rows occupies rows 7..=10, blank below. Verify
        // each expected piece exists somewhere in the body region.
        let found_brand = (0..=6).any(|r| vterm.row_text(r).contains("AtomCode"));
        let found_cwd = (0..=6).any(|r| vterm.row_text(r).contains("~/p/a"));
        let found_model = (0..=6).any(|r| vterm.row_text(r).contains("glm-5"));
        let found_hint = (0..=6).any(|r| vterm.row_text(r).contains("browse commands"));
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
        // Height 20 (not 18): the idle hints take one extra row since the
        // /webui shortcut was added, so the welcome block is one row taller.
        let (mut r, buf) = new_capturing(80, 20);

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
        let mut pre = crate::test_term::VirtualTerminal::new(80, 20);
        drain_into_vterm(&buf, &mut pre);

        r.on_resize(24, 20);
        r.flush_deferred();
        let mut post = crate::test_term::VirtualTerminal::new(24, 20);
        drain_into_vterm(&buf, &mut post);

        let brand_row = (0..20)
            .map(|row| post.row_text(row))
            .find(|row| row.contains("AtomCode"))
            .expect("brand row should remain visible after shrinking");
        let version_row = (0..20)
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

    /// Dark-theme color hierarchy: the `└` result line (leaf glyph +
    /// summary metadata) renders FAINT so it reads as subordinate to
    /// the bold tool-call header above it. On dark themes `Role::Muted`
    /// resolves to SGR 37 (near-white) which is visually indistinct
    /// from the header's default-fg; faint dims it to a gray, restoring
    /// the same two-tier hierarchy light theme gets for free from
    /// `MUTED_LIGHT` (DarkGrey). The header itself must stay bold and
    /// NOT faint — it's the prominent tier.
    #[test]
    fn retained_tool_result_summary_is_faint_in_dark_theme() {
        crate::highlight::theme::set_theme_mode(false); // dark
        let (mut r, buf) = new_capturing(80, 24);
        r.caps.colors = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::ToolCall {
            name: "ListDirectory".into(),
            detail: ".".into(),
        });
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

        // Result line: the `└` glyph and the summary text must be faint.
        let res_idx = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("└") && vterm.row_text(i).contains("3 files"))
            .unwrap_or_else(|| panic!("result row missing\ndump:\n{}", vterm.dump()));
        let arrow_cell = vterm.cell_at(res_idx, 2);
        assert_eq!(arrow_cell.ch, '└');
        assert!(
            arrow_cell.faint,
            "`└` glyph must be faint in dark theme, got {:?}",
            arrow_cell,
        );
        let summary_col = vterm.row_text(res_idx).find("3 files").unwrap();
        let summary_cell = vterm.cell_at(res_idx, summary_col);
        assert!(
            summary_cell.faint,
            "summary metadata must be faint in dark theme, got {:?}",
            summary_cell,
        );

        // Header line: the `●` anchor must be faint (subordinate tier,
        // in lockstep with the `└` line), while the tool name must be
        // bold and NOT faint — the prominent tier.
        let hdr_idx = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("ListDirectory"))
            .unwrap_or_else(|| panic!("header row missing\ndump:\n{}", vterm.dump()));
        let bullet_col = vterm.row_text(hdr_idx).find('●').unwrap();
        let bullet_cell = vterm.cell_at(hdr_idx, bullet_col);
        assert!(
            bullet_cell.faint,
            "`●` bullet must be faint in dark theme, got {:?}",
            bullet_cell,
        );
        let name_col = vterm.row_text(hdr_idx).find("ListDirectory").unwrap();
        let name_cell = vterm.cell_at(hdr_idx, name_col);
        assert!(name_cell.bold, "tool name must be bold, got {:?}", name_cell);
        assert!(
            !name_cell.faint,
            "tool name must NOT be faint, got {:?}",
            name_cell,
        );
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
    /// uses SGR-2 dim and leaves fg at terminal default.
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

        // Append-only top-anchored: spinner is the only body row,
        // sits at row 0; footer follows at rows 1..=4 (top_rule=1,
        // middle=2, bot_rule=3, status=4).
        let spinner_row = vterm.row_text(0);
        assert!(
            spinner_row.contains("⠋") && spinner_row.contains("Thinking"),
            "spinner not found on first body row (got {:?}):\n{}",
            spinner_row,
            vterm.dump()
        );
        // Frame glyph at absolute col 0 — flush-left with body paragraphs.
        assert_eq!(
            vterm.cell_at(0, 0).ch,
            '⠋',
            "expected frame at col 0, found {:?}:\n{}",
            vterm.cell_at(0, 0).ch,
            vterm.dump()
        );

        // Footer no longer hosts the spinner — the top_rule row
        // (which USED to share a slot with the spinner) must be empty
        // of any spinner glyphs.
        let top_rule_row = vterm.row_text(1);
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
        // repaint or resize.
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
        // Append-only: footer sits at body_rows_on_screen (= 0 here
        // since body_lines is empty), not at the screen bottom.
        let footer_top = r.body_lines.len().min(h.saturating_sub(footer_rows));
        // Layout: top_rule + middle×N + bot_rule + status (spinner no
        // longer reserves a footer row — lives in body now).
        // With 2-row middle: bot_rule at footer_top + 1 + 2 = footer_top + 3.
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
    /// all three glyphs reach the terminal. Earlier revisions pinned
    /// CONSECUTIVE emission (no CUP between wide chars) to validate
    /// the run-packing optimization. That optimization turned out to
    /// be the root cause of a Windows bug: `unicode-width`'s default
    /// narrow interpretation of East Asian Ambiguous characters
    /// disagreed with legacy conhost / xterm.js (which render them at
    /// 2 cols in CJK locale), so the model-predicted cursor drifted
    /// from the real cursor and subsequent patches landed at wrong
    /// columns. `serialize_patches` now forces a CUP before each
    /// patch following a non-ASCII cell (codepoint >= U+0080), so
    /// any width-prediction error self-corrects immediately. This
    /// test only asserts all three glyphs reach the stream — CUP
    /// interleaving between them is the desired new shape.
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
        for ch in ['你', '是', '谁'] {
            assert!(
                stream.contains(ch),
                "wide char {:?} missing from retained emit stream:\n{}",
                ch,
                stream
            );
        }
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

        // Close menu (Esc). Footer shrinks back to 4 — the cell-diff
        // path repaints. In append-only mode body is top-anchored so
        // welcome rows don't change row position; this just verifies
        // we don't accidentally duplicate them on the grid.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Welcome brand must appear EXACTLY once on the grid (top-
        // anchored layout makes ghost rows from the OLD bottom-anchored
        // re-paint impossible, but we keep the invariant pinned).
        let brand_rows = (0..24)
            .filter(|r| vterm.row_text(*r).contains("AtomCode"))
            .count();
        assert_eq!(
            brand_rows, 1,
            "menu-close: welcome brand should appear exactly once \
             (got {}):\n{}",
            brand_rows,
            vterm.dump()
        );
        // Use the `∙ ` prefix unique to the welcome cwd row so we
        // don't also match the status row's `model · cwd` glue.
        let cwd_rows = (0..24)
            .filter(|r| vterm.row_text(*r).contains("∙ ~/project"))
            .count();
        assert_eq!(
            cwd_rows, 1,
            "menu-close: welcome cwd should appear exactly once \
             (got {}):\n{}",
            cwd_rows,
            vterm.dump()
        );
    }

    /// Regression for user report: after `/model` switched providers,
    /// scrolling up showed the welcome banner + prior messages
    /// duplicated in scrollback. Root cause (historical, pre
    /// append-only refactor): `/model` changes the status-line text,
    /// which can change the footer height (status wraps, or
    /// spinner/menu rows differ between frames). When
    /// `current_footer_rows()` shifted, the DECSTBM
    /// shrunk/grew branches cleared the viewport and re-emitted every
    /// cached body row through `emit_body_line_inner` — which used
    /// `\n` at the region bottom, scrolling the top row into
    /// terminal scrollback. Any cached body row that had already
    /// entered scrollback during its original emit then entered a
    /// second time: a duplicate the user saw on scroll-up.
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

        // Fingerprint: welcome hint is unique. In the append-only
        // refactor scrollback isn't fed during overflow (cell-diff
        // visually shifts older rows off-screen instead of LF-scroll);
        // the invariant we still pin is "footer geometry changes
        // must not spuriously feed scrollback either".
        let hint = "to add a custom model";
        let count_hint = |vt: &crate::test_term::VirtualTerminal| {
            vt.scrollback_texts()
                .iter()
                .filter(|row| row.contains(hint))
                .count()
        };
        let baseline_hint = count_hint(&vterm);
        let sb_before = vterm.scrollback_len();

        // Footer height change: long buffer wraps the middle to 3
        // rows (text budget = 80 - 6 = 74 cols; 200 'x' → 3 rows).
        // The footer grows, body region shrinks. Cell-diff path
        // repaints — no LFs, no scrollback feed.
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

        // Hint count must not grow — anything > baseline is a
        // spurious re-feed during the footer transition.
        assert_eq!(
            count_hint(&vterm),
            baseline_hint,
            "footer growth duplicated welcome hint in scrollback \
             (was {}, now {}):\nscrollback:\n{}",
            baseline_hint,
            count_hint(&vterm),
            vterm.scrollback_texts().join("\n")
        );
        // Broader sanity: scrollback length must not grow during a
        // pure footer-geometry change.
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
    /// above the input box. Root cause (historical, pre append-only
    /// refactor): the DECSTBM resize path used `\x1b[2J` to wipe the
    /// viewport before re-painting the body.
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

        // Welcome fingerprint: `/login` is unique to the welcome
        // hint row and is a single non-wrapping token, so it gives a
        // stable single-row marker even when the combined hint line
        // soft-wraps at narrower widths. Must appear exactly once in
        // the *visible* viewport and zero times in scrollback.
        let hint = "/login";
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
        let line = "      \x1b[31m× GLM-5.1  (requires Pro plan or higher)\x1b[39m\n";
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
    fn retained_with_writer_does_not_enable_mouse_capture() {
        // Contract inverted: mouse capture is intentionally disabled at
        // startup so wheel / cmd+drag selection / cmd+C copy stay with
        // the terminal's native handling. Previously this test asserted
        // ?1002h/?1006h were emitted at startup; that behaviour was
        // removed by the "disable mouse capture, defer to terminal-
        // native selection/wheel" change.
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink(buf.clone());
        let r = RetainedRenderer::with_writer(sink, caps_with_color(), 80, 24);
        // Snapshot startup bytes BEFORE Drop emits the disable sequence.
        let startup = buf.lock().unwrap().clone();
        let s = String::from_utf8_lossy(&startup);
        assert!(
            !s.contains("\x1b[?1002h"),
            "startup must NOT enable button-event tracking — defer to terminal-native wheel/selection: {:?}",
            s
        );
        assert!(
            !s.contains("\x1b[?1006h"),
            "startup must NOT enable SGR mouse coords — defer to terminal-native wheel/selection: {:?}",
            s
        );
        drop(r);
        // Drop must still emit the disable sequence as a defensive
        // hygiene measure (clears any stale capture state inherited
        // from a prior process or panicked atomcode run).
        let after_drop_bytes = buf.lock().unwrap().clone();
        let after_drop = String::from_utf8_lossy(&after_drop_bytes);
        assert!(
            after_drop.contains("\x1b[?1002l"),
            "Drop must emit mouse-mode disable (1002l) defensively: {:?}",
            after_drop
        );
        assert!(
            after_drop.contains("\x1b[?1006l"),
            "Drop must emit mouse-mode disable (1006l) defensively: {:?}",
            after_drop
        );
    }

    #[test]
    fn retained_suspend_disables_mouse_capture() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink(buf.clone());
        let mut r = RetainedRenderer::with_writer(sink, caps_with_color(), 80, 24);
        buf.lock().unwrap().clear();
        r.suspend_for_external();
        let bytes = buf.lock().unwrap().clone();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[?1006l"), "suspend must disable SGR: {:?}", s);
        assert!(s.contains("\x1b[?1002l"), "suspend must disable button-event: {:?}", s);
    }

    #[test]
    fn retained_resume_does_not_reenable_mouse_capture() {
        // Contract inverted: resume must NOT re-enable mouse capture,
        // because startup never enabled it. Re-enabling here would
        // suddenly steal wheel events back from the terminal after the
        // user returns from an external subprocess (OAuth browser,
        // shell prompt), breaking the deferred-to-terminal contract.
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink(buf.clone());
        let mut r = RetainedRenderer::with_writer(sink, caps_with_color(), 80, 24);
        r.suspend_for_external();
        buf.lock().unwrap().clear();
        r.resume_from_external();
        let bytes = buf.lock().unwrap().clone();
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            !s.contains("\x1b[?1002h"),
            "resume must NOT re-enable button-event tracking — defer to terminal: {:?}",
            s
        );
        assert!(
            !s.contains("\x1b[?1006h"),
            "resume must NOT re-enable SGR mouse coords — defer to terminal: {:?}",
            s
        );
    }

    #[test]
    fn retained_shutdown_disables_mouse_capture() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink(buf.clone());
        let mut r = RetainedRenderer::with_writer(sink, caps_with_color(), 80, 24);
        // clear startup bytes
        buf.lock().unwrap().clear();
        r.shutdown();
        let bytes = buf.lock().unwrap().clone();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[?1002l"), "shutdown must disable button-event: {:?}", s);
        assert!(s.contains("\x1b[?1006l"), "shutdown must disable SGR coords: {:?}", s);
    }

    /// Pins the Issue-1 contract: when a mouse-wheel tick reaches
    /// `RetainedRenderer::scroll_body`, it must do nothing. We
    /// receive the event because `?1002h` (enabled for drag
    /// selection) consumes wheel ticks from the SGR mouse stream,
    /// but acting on the event would only shift OUR cell grid — the
    /// host terminal's scrollback would still be unreachable. The
    /// user-facing scrollback path is the terminal-level Shift+wheel
    /// / Cmd+↑ bypass, which the terminal resolves BEFORE the event
    /// hits our stdin.
    ///
    /// Regression guard: a future change that wires this method up
    /// to body movement would silently break "Shift+wheel scrolls
    /// the terminal" because the body would also jump, and would
    /// pollute scrollback because emit-side LFs would push stale
    /// rows into history.
    #[test]
    fn retained_scroll_body_is_noop_to_preserve_native_scrollback() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink(buf.clone());
        let mut r = RetainedRenderer::with_writer(sink, caps_with_color(), 80, 24);
        // Seed enough body content that any naive scroll attempt
        // would have something to emit (we want to assert that even
        // with rows present, scroll_body writes nothing).
        for i in 0..40 {
            r.render(UiLine::User(format!("L{}", i)));
        }
        r.flush_deferred();
        // Capture the byte count right BEFORE the wheel arrives so
        // anything emitted by the wheel call shows up as a delta.
        let before = buf.lock().unwrap().len();
        // Simulate a few wheel ticks in each direction.
        r.scroll_body(-3);
        r.scroll_body(3);
        r.scroll_body(-1);
        r.scroll_body(1);
        let after = buf.lock().unwrap().len();
        assert_eq!(
            before, after,
            "scroll_body must NOT write any bytes — wheel scroll belongs to the \
             terminal's native scrollback via Shift+wheel / Cmd+↑. Emitted {} \
             unexpected bytes.",
            after - before
        );
    }

    /// Contract inverted from the original `retained_drag_selection_
    /// Live scrollback feed: once body_lines exceeds the visible cap,
    /// every subsequent `emit_body_line_inner` must precede its own
    /// body-row emit with a "park at (h,1) + LF" sequence — that's the
    /// instruction terminals interpret as "scroll the entire visible
    /// area up by 1, top row enters native scrollback". The user
    /// experience this unlocks is `cmd+↑` / mouse-wheel during the
    /// atomcode session showing history above the live viewport.
    #[test]
    fn retained_overflow_pushes_oldest_to_scrollback() {
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(80, h);
        // Seed footer state so current_footer_rows() is stable.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        // Fill the body up to the cap (no overflow expected yet) so we
        // know exactly when the first scrollback feed should land.
        let cap = (h as usize).saturating_sub(r.current_footer_rows());
        for i in 0..cap {
            r.render(UiLine::AssistantText(format!("filler {}\n", i)));
        }
        r.flush_deferred();
        // Reset the capture so subsequent overflow events are isolated.
        buf.lock().unwrap().clear();
        // One more body line — body_lines.len() is now == cap at entry
        // to emit_body_line_inner, so it must trigger the bottom-LF
        // scroll BEFORE its own row emit.
        r.render(UiLine::AssistantText("overflow row\n".into()));
        r.flush_deferred();
        let captured = buf.lock().unwrap().clone();
        let s = String::from_utf8_lossy(&captured);
        let expected = format!("\x1b[{};1H\n", h);
        assert!(
            s.contains(&expected),
            "overflow emit must include bottom-row LF scroll trigger {:?}\nfull: {:?}",
            expected,
            s
        );
    }

    /// Regression for the user-reported "scrolling up shows duplicate
    /// content" bug on macOS Terminal.app (and reproduced on every
    /// emulator tested). Each unique body row must enter native
    /// scrollback at most ONCE across its lifetime, even if intermediate
    /// tail pops (spinner clear, approval pop, ImageAttachment, inflight
    /// commit) make `body_lines.len()` re-cross the cap threshold.
    ///
    /// Mechanism the bug exploits: after an overflow LF promotes row R
    /// to native scrollback, R remains at the front of `body_lines`.
    /// When `body_lines.last()` is popped, `start = len - cap` decreases,
    /// re-exposing R at viewport row 0. The next push that overflows
    /// then LFs R into scrollback a SECOND time — duplicate.
    ///
    /// Repro sequence: fill body to exactly `cap`, push spinner
    /// (overflow #1 — promotes the oldest body row), clear spinner via
    /// InputPrompt (pops the spinner), push one more body row
    /// (overflow #2 — under the bug, re-promotes the same oldest row).
    #[test]
    fn retained_spinner_pop_does_not_duplicate_scrollback() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        let status = status_basic();

        // Seed footer geometry so `cap` is stable.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let cap = (h as usize).saturating_sub(r.current_footer_rows());

        // Fill body to exactly cap (no overflow yet — each push has
        // body_lines.len() < cap at entry to emit_body_line_inner).
        // Use a uniquely-identifiable first row so we can count its
        // appearances in scrollback.
        let probe = "PROBE-ROW-XYZ";
        r.render(UiLine::AssistantText(format!("{}\n", probe)));
        for i in 1..cap {
            r.render(UiLine::AssistantText(format!("filler-{:03}\n", i)));
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Spinner push: body_lines.len() == cap, so emit_body_line_inner
        // takes the overflow branch and LFs the PROBE row into native
        // scrollback. Count after this should be exactly 1.
        r.render(UiLine::Spinner {
            frame: "⠋".into(),
            label: "Pondering…".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let count_probe = |vt: &crate::test_term::VirtualTerminal| -> usize {
            vt.scrollback_texts()
                .iter()
                .filter(|row| row.contains(probe))
                .count()
        };
        assert_eq!(
            count_probe(&vterm),
            1,
            "after first overflow PROBE should be in scrollback exactly once; got {}.\nscrollback:\n{}",
            count_probe(&vterm),
            vterm.scrollback_texts().join("\n")
        );

        // Idle InputPrompt triggers clear_live_spinner → pops the
        // transient spinner row. body_lines.len() now drops from cap+1
        // back to cap. With the bug, the next push will treat the front
        // row (still PROBE) as if it had never been promoted.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Next body push — overflow #2. With the bug, viewport row 0
        // is PROBE again (because start went from 1 back to 0 after the
        // spinner pop), so the LF re-promotes it.
        r.render(UiLine::AssistantText("after-pop\n".into()));
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        assert_eq!(
            count_probe(&vterm),
            1,
            "PROBE duplicated in scrollback after spinner-pop + push (count={}).\nscrollback:\n{}",
            count_probe(&vterm),
            vterm.scrollback_texts().join("\n")
        );
    }

    /// Regression for the user-reported "30x duplicate bullet in
    /// scrollback when expanding window mid-stream" bug on macOS
    /// Terminal.app. The model streamed each bullet exactly once
    /// (SSE wire dump verified), but the user expanded the terminal
    /// window during the stream and saw ~30 copies of the FIRST
    /// bullet in native scrollback.
    ///
    /// Mechanism this test pins: every overflow LF must promote a
    /// UNIQUE row to scrollback, even when resize events arrive
    /// interleaved with streaming pushes. Each row's payload should
    /// appear in scrollback at most once across the entire session,
    /// regardless of resize cadence.
    #[test]
    fn retained_stream_with_resize_larger_does_not_duplicate_scrollback() {
        let w_small: u16 = 67;
        let h_small: u16 = 24;
        let w_large: u16 = 67;
        let h_large: u16 = 41;
        let (mut r, buf) = new_capturing(w_small, h_small);
        // VTerm large enough to absorb post-resize CUPs; we'll cross-
        // check scrollback duplicates at end.
        let mut vterm = crate::test_term::VirtualTerminal::new(w_large, h_large);
        let status = status_basic();

        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let cap_small = (h_small as usize).saturating_sub(r.current_footer_rows());

        // Phase 1 — stream PAST the small cap so several overflow LFs
        // fire and earlier rows promote to native scrollback. Tag the
        // FIRST bullet uniquely so we can count its scrollback copies.
        let probe = "BULLET-FIRST-ZZZ";
        r.render(UiLine::AssistantText(format!("{}\n", probe)));
        for i in 0..(cap_small + 5) {
            r.render(UiLine::AssistantText(format!(
                "filler-bullet-{:03}\n",
                i
            )));
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let count_probe = |vt: &crate::test_term::VirtualTerminal| -> usize {
            vt.scrollback_texts()
                .iter()
                .filter(|row| row.contains(probe))
                .count()
        };
        // After phase 1, PROBE has been promoted to scrollback (the
        // overflow ran past it). Must be exactly one copy.
        let phase1 = count_probe(&vterm);
        assert!(
            phase1 <= 1,
            "phase 1 already duplicated PROBE (count={}).\nscrollback:\n{}",
            phase1,
            vterm.scrollback_texts().join("\n")
        );

        // Phase 2 — resize larger MID-STREAM. This mirrors the user
        // dragging the bottom edge of their Terminal.app window. The
        // SIGWINCH coalescer in event_loop dispatches one on_resize
        // with the final geometry; the renderer must repaint without
        // re-LFing already-promoted rows.
        r.on_resize(w_large, h_large);
        drain_into_vterm(&buf, &mut vterm);

        // Phase 3 — streaming continues on the now-larger window.
        // These pushes happen on the larger cap, but `scrolled_off`
        // must still correctly account for rows already in scrollback
        // so the next overflow (whenever it lands) promotes a UNIQUE
        // new row, never PROBE again.
        for i in 0..30 {
            r.render(UiLine::AssistantText(format!(
                "post-resize-bullet-{:03}\n",
                i
            )));
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let count = count_probe(&vterm);
        assert!(
            count <= 1,
            "PROBE duplicated in scrollback after resize-larger + stream (count={}).\nscrollback head:\n{}",
            count,
            vterm
                .scrollback_texts()
                .iter()
                .take(50)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Stress test: spinner ticks INTERLEAVED with streaming pushes,
    /// then a resize-larger lands mid-stream while live_spinner_active
    /// is still true (model still working). Mimics the user's exact
    /// repro: "expand window during output". Each unique row must
    /// promote to scrollback at most once.
    #[test]
    fn retained_streaming_spinner_resize_does_not_duplicate_scrollback() {
        let w_small: u16 = 67;
        let h_small: u16 = 24;
        let w_large: u16 = 67;
        let h_large: u16 = 41;
        let (mut r, buf) = new_capturing(w_small, h_small);
        let mut vterm = crate::test_term::VirtualTerminal::new(w_large, h_large);
        let status = status_basic();

        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let cap_small = (h_small as usize).saturating_sub(r.current_footer_rows());

        // Stream a section header + a tagged "first bullet" + filler
        // bullets, with spinner ticks sprinkled in between (mimics
        // StreamingBox / Spinner events arriving during streaming).
        let probe = "BULLET-FIRST-RESIZE";
        r.render(UiLine::AssistantText("🛠️ 技术特色\n".into()));
        r.render(UiLine::Spinner {
            frame: "⠋".into(),
            label: "Pondering…".into(),
        });
        r.render(UiLine::AssistantText(format!("{}\n", probe)));
        r.render(UiLine::Spinner {
            frame: "⠙".into(),
            label: "Pondering…".into(),
        });
        // Push past the small cap to trigger several overflow LFs.
        for i in 0..(cap_small + 10) {
            r.render(UiLine::AssistantText(format!(
                "filler-bullet-{:03}\n",
                i
            )));
            if i % 3 == 0 {
                r.render(UiLine::Spinner {
                    frame: "⠹".into(),
                    label: "Pondering…".into(),
                });
            }
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Mid-stream RESIZE LARGER. live_spinner_active is true.
        r.on_resize(w_large, h_large);
        drain_into_vterm(&buf, &mut vterm);

        // Keep streaming + spinner ticks after the resize.
        for i in 0..50 {
            r.render(UiLine::AssistantText(format!(
                "post-resize-bullet-{:03}\n",
                i
            )));
            if i % 2 == 0 {
                r.render(UiLine::Spinner {
                    frame: "⠸".into(),
                    label: "Pondering…".into(),
                });
            }
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let count = vterm
            .scrollback_texts()
            .iter()
            .filter(|row| row.contains(probe))
            .count();
        assert!(
            count <= 1,
            "PROBE duplicated in scrollback across streaming + spinner + resize (count={}).\nscrollback head (first 40 lines):\n{}",
            count,
            vterm
                .scrollback_texts()
                .iter()
                .take(40)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Sub-cap pushes must NOT touch the bottom row — that would scroll
    /// content the user can still see into native scrollback prematurely,
    /// duplicating rows between the visible area and the scrollback view.
    /// The bottom-LF is reserved for genuine overflow moments only.
    #[test]
    fn retained_below_cap_does_not_scroll_terminal() {
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(80, h);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let cap = (h as usize).saturating_sub(r.current_footer_rows());
        // Push strictly fewer body rows than cap so emit_body_line_inner
        // never enters the overflow branch.
        let push_count = cap.saturating_sub(2);
        buf.lock().unwrap().clear();
        for i in 0..push_count {
            r.render(UiLine::AssistantText(format!("row {}\n", i)));
        }
        r.flush_deferred();
        let captured = buf.lock().unwrap().clone();
        let s = String::from_utf8_lossy(&captured);
        let forbidden = format!("\x1b[{};1H\n", h);
        assert!(
            !s.contains(&forbidden),
            "below-cap pushes must not emit bottom-LF scroll {:?}\nfull: {:?}",
            forbidden,
            s
        );
    }

    /// After overflow has happened, the visible body region must still
    /// show the MOST RECENT body_height rows of `body_lines`, with the
    /// footer immediately below — i.e. the scroll-up-then-repaint dance
    /// must leave the on-screen state identical to "if the cap had never
    /// been exceeded and we just painted the tail directly". Without the
    /// `shift_prev_up` sync the cell-diff would produce a smeared frame
    /// (stale top row + duplicate row at the bottom).
    #[test]
    fn retained_overflow_keeps_visible_body_correct() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let cap = (h as usize).saturating_sub(r.current_footer_rows());
        // Push 2x cap rows to guarantee overflow events have fired.
        for i in 0..(cap * 2) {
            r.render(UiLine::AssistantText(format!("R{:03}\n", i)));
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // The body_lines tail's most recent labels MUST be visible on
        // the live viewport rows [0..body_rows_on_screen). The very last
        // row before the footer should show the latest pushed label.
        let body_rows_on_screen = r.body_bottom_row() as usize;
        assert!(
            body_rows_on_screen > 0,
            "body should occupy at least one row after overflow"
        );
        // Take a sample of the most recent labels and confirm each is
        // present somewhere in the live body region.
        let total = r.body_lines.len();
        // The body_lines that actually live in the screen tail.
        let tail_start = total.saturating_sub(body_rows_on_screen);
        let mut seen_recent = 0;
        for idx in tail_start..total {
            // Recover the label by scanning body_lines for the printable chars.
            let label: String = r.body_lines[idx]
                .iter()
                .filter(|c| c.width > 0)
                .map(|c| c.ch)
                .collect();
            let label = label.trim().to_string();
            if label.is_empty() {
                continue;
            }
            let found = (0..body_rows_on_screen).any(|row| vterm.row_text(row).contains(&label));
            if found {
                seen_recent += 1;
            }
        }
        assert!(
            seen_recent >= 1,
            "expected at least one recent body row visible on screen after overflow; \
             body_rows_on_screen={} total={}\ndump:\n{}",
            body_rows_on_screen,
            total,
            vterm.dump()
        );
        // The oldest row (R000) MUST NOT be in the live viewport
        // (it should have been scrolled out into scrollback).
        let oldest_visible_in_live =
            (0..body_rows_on_screen).any(|row| vterm.row_text(row).contains("R000"));
        assert!(
            !oldest_visible_in_live,
            "oldest body row R000 must be off-screen (scrolled into native scrollback) \
             after pushing 2*cap rows\ndump:\n{}",
            vterm.dump()
        );
    }

    /// Regression (top-anchored body model): `commit_inflight_tool`
    /// previously read `body_bottom_row()` AFTER truncating the
    /// inflight rows, so the erase range pointed at the LAST `remove`
    /// rows of REMAINING body content rather than the rows that
    /// formerly held the spinner. Cell-diff could not catch the
    /// regression because the Screen buffer state matched the body
    /// (we erased the physical terminal but left buffer marks intact).
    /// User-visible bug: prior body markers vanished from the terminal
    /// after every tool commit when the body had pre-existing content.
    #[test]
    fn retained_commit_inflight_preserves_prior_body() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        let status = status_basic();

        // Seed a full frame so footer is painted (mirrors real boot).
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Push N markers as body rows BEFORE the inflight starts.
        const N: usize = 5;
        for i in 0..N {
            r.render(UiLine::AssistantText(format!("MARKER_R{}\n", i)));
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Trigger inflight (adds rows AFTER the markers).
        r.render(UiLine::ToolCallInFlight {
            id: "call-1".into(),
            name: "Bash".into(),
            detail: "ls -la /tmp".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Commit (truncates inflight rows — must NOT clobber markers).
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-1".into()),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // All N markers must still be visible on the terminal.
        for i in 0..N {
            let label = format!("MARKER_R{}", i);
            assert!(
                vterm.any_row(|row| row.contains(&label)),
                "MARKER_R{} should still be on terminal after commit_inflight_tool, \
                 but it was erased by the over-erase bug.\ndump:\n{}",
                i,
                vterm.dump()
            );
        }
    }

    /// Regression (top-anchored body model): `pop_approval_prompt`
    /// has the same wrong-row-erase pattern as `commit_inflight_tool`,
    /// but the bug is masked at the user level by the immediately
    /// following `screen.invalidate()` that forces a full repaint.
    /// This test still asserts the post-condition contract — that
    /// prior body rows remain visible after popping — so a future
    /// refactor that drops the `invalidate()` shield can't silently
    /// regress this path. With the over-erase removed (or the math
    /// corrected) the test passes for the right reason instead of
    /// being saved by the invalidation hammer.
    #[test]
    fn retained_approval_pop_preserves_prior_body() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        let status = status_basic();

        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        const N: usize = 5;
        for i in 0..N {
            r.render(UiLine::AssistantText(format!("MARKER_R{}\n", i)));
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        r.render(UiLine::ApprovalPrompt {
            tool: "bash".into(),
            detail: "ls".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        r.pop_approval_prompt();
        // Trigger a repaint cycle (mirrors what happens after Y/A/N).
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        for i in 0..N {
            let label = format!("MARKER_R{}", i);
            assert!(
                vterm.any_row(|row| row.contains(&label)),
                "MARKER_R{} should still be on terminal after pop_approval_prompt.\
                 \ndump:\n{}",
                i,
                vterm.dump()
            );
        }
    }

    /// Bug repro: user reports that assistant text rows duplicate in
    /// the scrollback right before an approval-needed tool call,
    /// visible on every terminal (macOS Terminal.app, iTerm2, VSCode
    /// pwsh xterm.js) by scrolling up. Wire-dump confirms the LLM
    /// only emits each line once. So the duplicate is in body_lines.
    ///
    /// This test simulates the most-likely event sequence under load
    /// — TextDelta chunks token-by-token interleaved with spinner
    /// ticks, then a ToolCallStreaming → ToolCallStarted → ApprovalNeeded
    /// burst — and asserts that body_lines never contains two
    /// adjacent entries with the same content.
    #[test]
    fn retained_approval_pending_no_duplicate_body_rows() {
        let (mut r, _buf) = new_capturing(80, 24);

        // Phase 1: stream assistant text in token-sized chunks.
        // Real provider streams ~5-30 chars per delta. Insert a
        // spinner tick every few chunks (mirrors draw_spinner_now
        // 100ms cadence). The model output for this scenario:
        //   "副本编辑成功、读回一致、删除 - PASS。\n\n步骤 7 - `search_replace`\n"
        // followed by a function-calling tool_call (no XML in text).
        let chunks = [
            "副本", "编辑", "成功", "、读回", "一致",
            "、删除 - ", "PASS", "。\n\n",
            "步骤 7", " - ", "`search_replace`", "\n",
        ];
        for (i, chunk) in chunks.iter().enumerate() {
            r.render(UiLine::AssistantText((*chunk).into()));
            // Spinner tick every 3 chunks to mirror real cadence
            // (one tick per ~100ms, deltas arrive ~30ms apart).
            if i % 3 == 0 {
                r.render(UiLine::StreamingBox {
                    buf: String::new(),
                    cursor_byte: 0,
                    frame: "⠋",
                    label: "Pondering · 1s".to_string(),
                    status: status_basic(),
                    menu: None,
                    attachments: Vec::new(),
                });
            }
        }
        r.flush_deferred();

        // Phase 2: tool_call_started fires.
        r.render(UiLine::AssistantLineBreak);
        r.render(UiLine::ToolCallInFlight {
            id: "call-7".into(),
            name: "WriteFile".into(),
            detail: "atomcode_smoke_replace.txt".into(),
        });
        // Spinner ticks for the inflight tool.
        for frame in ["⠋", "⠙", "⠹"] {
            r.render(UiLine::StreamingBox {
                buf: String::new(),
                cursor_byte: 0,
                frame,
                label: "Pondering · 1s".to_string(),
                status: status_basic(),
                menu: None,
                attachments: Vec::new(),
            });
        }
        r.flush_deferred();

        // Phase 3: ApprovalNeeded fires — commit inflight + prompt.
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-7".into()),
        });
        r.render(UiLine::ApprovalPrompt {
            tool: "WriteFile".into(),
            detail: "atomcode_smoke_replace.txt".into(),
        });
        r.flush_deferred();

        // Assert no two adjacent rows have identical non-blank content.
        let row_texts: Vec<String> = r
            .body_lines
            .iter()
            .map(|row| row.iter().map(|c| c.ch).collect::<String>())
            .collect();
        for w in row_texts.windows(2) {
            let a = w[0].trim();
            let b = w[1].trim();
            if a.is_empty() || b.is_empty() {
                continue;
            }
            assert_ne!(
                a, b,
                "adjacent body_lines must not be identical (duplicate-row bug). \
                 a={:?} b={:?}",
                a, b
            );
        }

        // Specific assertion: "步骤 7" only appears once.
        let step_7_count = row_texts.iter().filter(|t| t.contains('步')).count();
        assert_eq!(
            step_7_count, 1,
            "step row must appear exactly once in body_lines, found {}: {:?}",
            step_7_count, row_texts
        );
    }

    /// Regression for the "missing top rule after /model switch" bug.
    ///
    /// Reproduction sequence (mirrors the real /model flow):
    ///   1. Steady-state InputPrompt (no menu) — paint, drain.
    ///   2. Open the slash menu (4 items) via InputPrompt with menu=Some —
    ///      paint, drain. Footer grows from 4 rows to 8 (top+middle+bot+
    ///      4 menu + status).
    ///   3. Close the menu via InputPrompt with menu=None AND immediately
    ///      push a body row ("已切换到 …") in the SAME tick — no
    ///      flush_deferred between the two so they coalesce into one
    ///      paint cycle.
    ///   4. flush_deferred → drain into vterm.
    ///
    /// Expected post-condition: the footer's top_rule lives at the row
    /// immediately above the input prompt `>`, and reads as a row full of
    /// `─` (or at least starts with `─`). The bug renders this row blank
    /// because `emit_body_line_inner` wrote the body row + LF directly to
    /// the terminal (advancing cursor past footer_top), then the next
    /// paint's cell-diff suppressed the top_rule emit on the rows whose
    /// prev_cells still held the (now-stale) menu content from before the
    /// close.
    #[test]
    fn retained_top_rule_visible_after_menu_close_and_body_push() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        let status = status_basic();

        // Step 1: baseline InputPrompt, no menu.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Step 2: open the slash menu (4 items, mirrors /model entry).
        let items: Vec<(String, String)> = vec![
            ("model".into(), "Switch model".into()),
            ("provider".into(), "Add provider".into()),
            ("session".into(), "New session".into()),
            ("resume".into(), "Resume session".into()),
        ];
        r.render(UiLine::InputPrompt {
            buf: "/model".into(),
            cursor_byte: 6,
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

        // Step 3a: user selects an item — menu closes via InputPrompt with
        // menu=None. State is updated but we DON'T flush_deferred yet —
        // in the real /model path the slash-handler immediately follows
        // up with the "已切换到 …" body row in the same event_loop tick.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });

        // Step 3b: slash handler emits the confirmation body row. This
        // is the line the user actually saw on screen.
        r.render(UiLine::CommandOutput(
            "  已切换到 AtomGit-deepseek-v4-flash · deepseek-v4-flash\n".into(),
        ));

        // Step 4: coalesce + paint + drain.
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Post-condition: locate the input-prompt row (the one starting
        // with the chevron) and assert the row IMMEDIATELY ABOVE it is a
        // full-width rule. With unicode + colors enabled, the chevron is
        // '❯'. Pad column varies, so we search the whole grid.
        let prompt_row = (0..h as usize)
            .find(|&row| {
                let text = vterm.row_text(row);
                text.contains('\u{276f}')
            })
            .unwrap_or_else(|| {
                panic!(
                    "no prompt-bearing row found post-menu-close;\ndump:\n{}",
                    vterm.dump()
                )
            });
        assert!(
            prompt_row >= 1,
            "prompt row {} has no row above it for top_rule;\ndump:\n{}",
            prompt_row,
            vterm.dump()
        );
        let top_rule_row = prompt_row - 1;
        let dashes = (0..w as usize)
            .filter(|&c| vterm.cell_at(top_rule_row, c).ch == '\u{2500}')
            .count();
        assert!(
            dashes >= (w as usize / 2),
            "top_rule above prompt row {} (i.e. row {}) is blank/short — only {} '─' dashes.\n\
             top_rule row text: {:?}\n\
             dump:\n{}",
            prompt_row,
            top_rule_row,
            dashes,
            vterm.row_text(top_rule_row),
            vterm.dump()
        );
    }

    /// Regression: body rows painted via `paint_body_into_cells` were
    /// only writing `clipped.len()` cells into `screen.cells[i]`, leaving
    /// the trailing `[clipped.len(), width)` columns untouched. The diff
    /// against `prev_cells` then emitted blanks correctly... in theory.
    ///
    /// In practice — and only when the body overflows the visible cap so
    /// `emit_body_line_inner`'s scroll-then-shift_prev_up path runs — the
    /// shift rotates `prev_cells` such that the new logical body row's
    /// slot in `prev_cells` holds the SAME body row's content from the
    /// last frame (the prior row shifted into its position is its own
    /// previous representation). So the diff sees "row content
    /// unchanged" for the slot and emits zero patches for it — including
    /// no blanks for the trailing columns that the body row never wrote.
    ///
    /// When the body line is then REPLACED with shorter content (e.g.
    /// `commit_inflight_tool` swapping a wrapped 3-row spinner for a
    /// single committed row; the spinner row was 60-cols wide, the
    /// commit row is 24-cols), the right-edge fragment of the old row
    /// stays on the physical terminal because no blank patches were
    /// emitted for those columns.
    ///
    /// Reproduction here is more direct: push a LONG body row, paint;
    /// then directly replace `body_lines.last()` with a SHORT row (the
    /// same shape `push_or_update_live_spinner`'s in-place update does),
    /// re-paint, and verify the right edge of the row is BLANK in the
    /// vterm grid — not the trailing fragment of the long row.
    #[test]
    fn retained_short_row_replacing_long_row_clears_trailing_cells_via_vterm() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        let status = status_basic();

        // Frame 1: seed the footer.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Push a LONG body row that fits on a single terminal row
        // (body width on an 80-col terminal with no scrollbar is 80 - 4
        // = 76 cells after `push_body_text`'s PAD_COL=2 padding on each
        // side; pick a payload that lands a unique trailing marker near
        // the right edge so we can spot it later).
        let long_marker = "ENDMARKER";
        let long_payload =
            format!("write_file(/long/path/to/file.md) {}", long_marker);
        // Sanity: ensure the payload fits in a single body row so we
        // don't accidentally land the marker on a wrapped second row.
        assert!(
            long_payload.len() <= (w as usize - 2 * 2),
            "long payload must fit one body row to make the test deterministic"
        );
        r.render(UiLine::CommandOutput(long_payload.clone()));
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Locate the body row that holds our long content on the
        // physical terminal grid.
        let body_height = r.body_bottom_row() as usize;
        let long_screen_row = (0..body_height)
            .find(|&row_0idx| vterm.row_text(row_0idx).contains(long_marker))
            .unwrap_or_else(|| {
                panic!(
                    "long row should be visible in vterm\nbody_height={}, body_lines.len()={}\ndump:\n{}",
                    body_height,
                    r.body_lines.len(),
                    vterm.dump()
                )
            });

        // Replace the in-memory body row with a SHORTER row and re-paint
        // via the normal paint pipeline. (Mirrors the shape of
        // `commit_inflight_tool` and similar paths that swap a long
        // wrapped row for a short committed one without going through
        // emit_body_line_inner / EL erase.)
        let short = {
            let mut row: Vec<Cell> = Vec::new();
            push_str_cells(&mut row, "  write_file(file.md)", &CellStyle::default());
            row
        };
        if let Some(last) = r.body_lines.last_mut() {
            *last = short;
        }
        // Mark dirty so the next flush actually runs paint_frame.
        r.dirty = true;
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Assert: the marker from the long row must be GONE from the
        // physical terminal — the trailing cells where it lived must
        // have been blanked by the paint cycle.
        let row_text = vterm.row_text(long_screen_row);
        assert!(
            !row_text.contains(long_marker),
            "ghost fragment {:?} still on screen at row {}\nrow text: {:?}\ndump:\n{}",
            long_marker,
            long_screen_row,
            row_text,
            vterm.dump()
        );
    }

    /// Regression for the ghost-row-tail bug seen in the user
    /// screenshot. The root cause is a multi-step interaction:
    ///   1. A body row is painted with LONG content. The cell-diff
    ///      emits the LONG cells; after the swap, `prev_cells` holds
    ///      `[LONG content][trailing blanks]` and physical terminal
    ///      mirrors that exactly.
    ///   2. Something invokes `screen.invalidate()` (or a path that
    ///      effectively zeroes the relevant `prev_cells` rows) — the
    ///      cell-diff cache now claims "no content anywhere", but the
    ///      physical terminal is unchanged.
    ///   3. The body row is replaced with SHORTER content and the
    ///      next paint runs. `paint_body_into_cells` only writes the
    ///      short cells into `screen.cells[i][0..short_len]`, leaving
    ///      `cells[i][short_len..w]` blank from `screen.clear()`.
    ///   4. The diff for trailing cols sees `cells = blank` and
    ///      `prev_cells = blank` (post-invalidate) → no patch emitted.
    ///      Physical terminal STILL holds `[LONG content tail]` at
    ///      those cols → user sees `[SHORT][ghost trail of LONG]`.
    ///
    /// `paint_body_into_cells` must pad each body row to the
    /// effective body width before `draw_row` and the invalidate
    /// callers must physically erase the affected rows; together this
    /// keeps cells / prev_cells / terminal byte-aligned across the
    /// invalidate-then-shrink boundary.
    #[test]
    fn retained_paint_body_clears_trailing_after_invalidate_with_shorter_row_via_vterm() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        let status = status_basic();

        // Seed the footer.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Frame 1: push a LONG body row through the normal paint path.
        let long_marker = "LONG_TAIL_ZZZ";
        let mut long_row: Vec<Cell> = Vec::new();
        push_str_cells(
            &mut long_row,
            &format!("body_payload {}", long_marker),
            &CellStyle::default(),
        );
        // Extend to a near-full-width payload so the marker lands well
        // past where any subsequent short row would reach.
        let target_len = (w as usize).saturating_sub(2);
        while long_row.iter().map(|c| c.width as usize).sum::<usize>() < target_len {
            long_row.push(Cell {
                ch: 'X',
                style: CellStyle::default(),
                width: 1,
            });
        }
        r.push_body_row(long_row);
        r.dirty = true;
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Locate the row on the physical grid.
        let body_height = r.body_bottom_row() as usize;
        let long_screen_row = (0..body_height)
            .find(|&row_0idx| vterm.row_text(row_0idx).contains(long_marker))
            .unwrap_or_else(|| {
                panic!(
                    "long row should be visible in vterm\nbody_lines.len()={} body_height={}\ndump:\n{}",
                    r.body_lines.len(),
                    body_height,
                    vterm.dump()
                )
            });

        // Frame 2: simulate the invalidate-then-shrink sequence the
        // bug requires. `pop_approval_prompt` / `refresh_welcome_banner`
        // both call `screen.invalidate()` to force a cold-start repaint
        // — verify the bug class by invoking invalidate directly so the
        // test isolates the rendering invariant (not the specific
        // caller).
        r.screen.invalidate();
        // Replace `body_lines.last()` with a much shorter row.
        let mut short_row: Vec<Cell> = Vec::new();
        push_str_cells(&mut short_row, "tiny", &CellStyle::default());
        if let Some(last) = r.body_lines.last_mut() {
            *last = short_row;
        }
        r.dirty = true;
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // The trailing cells must be blank — no ghost trail from the
        // LONG row.
        let after = vterm.row_text(long_screen_row);
        assert!(
            !after.contains(long_marker),
            "trailing fragment {:?} survived at row {} after invalidate+shrink: {:?}\ndump:\n{}",
            long_marker,
            long_screen_row,
            after,
            vterm.dump()
        );
        let right_edge = vterm.cell_at(long_screen_row, (w - 5) as usize);
        assert_eq!(
            right_edge.ch, ' ',
            "right-edge cell at row {} col {} should be blank, got {:?}\ndump:\n{}",
            long_screen_row,
            w - 5,
            right_edge,
            vterm.dump()
        );
    }

    /// Variant of the test above that DIRECTLY exercises the
    /// `paint_body_into_cells` short-row-after-long-row case at the
    /// screen.cells layer (no `body_lines.last_mut()` shenanigans),
    /// driving the paint pipeline twice via `flush_deferred()`
    /// against a body_lines slot whose content was just shortened.
    #[test]
    fn retained_paint_body_short_row_clears_trailing_cells_via_vterm() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        let status = status_basic();

        // Seed the footer.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Push a long body row directly. Use a uniquely identifiable
        // tail marker.
        let long_marker = "RIGHT_EDGE_TAIL_ZZZ";
        let mut long_row: Vec<Cell> = Vec::new();
        push_str_cells(
            &mut long_row,
            &format!("short_payload {}", long_marker),
            &CellStyle::default(),
        );
        // Force the row to be exactly the full effective body width by
        // padding with trailing spaces — same shape a CommandOutput row
        // would have when it exhausts the wrap budget. This is the case
        // where prev_cells would otherwise be wide enough that the diff
        // sees changes across the WHOLE row when we shorten.
        let target_len = (w as usize).saturating_sub(2 * 2);
        while long_row.iter().map(|c| c.width as usize).sum::<usize>() < target_len {
            long_row.push(Cell {
                ch: 'X',
                style: CellStyle::default(),
                width: 1,
            });
        }
        r.push_body_row(long_row);
        r.dirty = true;
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // The long row should be on the screen.
        let body_height = r.body_bottom_row() as usize;
        let long_screen_row = (0..body_height)
            .find(|&row_0idx| vterm.row_text(row_0idx).contains(long_marker))
            .unwrap_or_else(|| {
                panic!(
                    "long row should be visible in vterm\nbody_lines.len()={} body_height={}\ndump:\n{}",
                    r.body_lines.len(),
                    body_height,
                    vterm.dump()
                )
            });

        // Replace `body_lines.last()` with a much shorter row.
        let mut short_row: Vec<Cell> = Vec::new();
        push_str_cells(&mut short_row, "tiny", &CellStyle::default());
        if let Some(last) = r.body_lines.last_mut() {
            *last = short_row;
        }
        r.dirty = true;
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // The trailing cells of long_screen_row must be wiped.
        let after = vterm.row_text(long_screen_row);
        assert!(
            !after.contains(long_marker),
            "trailing fragment {:?} survived at row {}: {:?}\ndump:\n{}",
            long_marker,
            long_screen_row,
            after,
            vterm.dump()
        );
        // Also: cells past the short row's content should be SPACE.
        let cell_at_right_edge = vterm.cell_at(long_screen_row, (w - 5) as usize);
        assert_eq!(
            cell_at_right_edge.ch, ' ',
            "right-edge cell at row {} col {} should be blank, got {:?}\ndump:\n{}",
            long_screen_row,
            w - 5,
            cell_at_right_edge,
            vterm.dump()
        );
    }

    /// Defense-in-depth: no cell in a Java code block (after the full
    /// `AssistantText` → `flush_assistant_remainder` →
    /// `push_markdown_body` → `parse_markdown_to_cells` pipeline) may
    /// carry `style.reverse == true`. This is the guard against any
    /// future change that lets reverse-video bleed onto syntax-
    /// highlighted code (the "green background blocks" symptom the user
    /// reported when a stray SGR 7 from earlier output couldn't be
    /// cleared by the historical `theme::RESET = "\x1b[23;39m"` — that
    /// only cleared italic + fg, not reverse. `RESET` has since been
    /// promoted to a full SGR 0 reset; see
    /// `markdown_stream_does_not_leak_reverse_across_tokens` below for
    /// the unit-level proof.).
    ///
    /// Current code already passes this — none of the syntect, markdown,
    /// or render paths emit SGR 7 onto body cells; the test exists as
    /// a regression net so any future addition of `\x1b[7m` upstream
    /// of `parse_markdown_to_cells` trips here instead of silently
    /// rendering reverse-video on every code-block cell.
    #[test]
    fn retained_java_code_block_has_no_reverse_video_cells() {
        let (mut r, _buf) = new_capturing(80, 24);
        let stream = "Here is some Java:\n\
                     \n\
                     ```java\n\
                     int[] arr = {0, 1, 4, 3};\n\
                     String s = null;\n\
                     void foo(int x, Object y) { return null; }\n\
                     ```\n\
                     \n\
                     Closing prose.\n";
        r.render(UiLine::AssistantText(stream.into()));
        r.render(UiLine::AssistantLineBreak);

        let mut reverse_cells: Vec<(usize, usize, char)> = Vec::new();
        for (row_idx, row) in r.body_lines.iter().enumerate() {
            for (col_idx, cell) in row.iter().enumerate() {
                if cell.style.reverse {
                    reverse_cells.push((row_idx, col_idx, cell.ch));
                }
            }
        }
        assert!(
            reverse_cells.is_empty(),
            "no body cell from a syntax-highlighted code block may carry \
             reverse-video — found {:?}. The class of bug this guards: \
             SGR 7 leaks in from earlier output and the highlighter's \
             RESET (`\\x1b[23;39m`) doesn't clear reverse, so the entire \
             code block renders with swapped fg/bg (green-on-default for \
             string tokens, etc.).",
            reverse_cells,
        );
    }

    /// Unit-level regression for the `theme::RESET` under-clearing bug.
    ///
    /// Pre-fix `RESET` was `\x1b[23;39m` (italic-off + default-fg only).
    /// When upstream UI chrome (ApprovalPrompt Y chip, top-rule session
    /// pill, etc.) emitted `\x1b[7m` and never explicitly disabled it
    /// with SGR 27, the working `style` inside `parse_markdown_to_cells`
    /// carried `reverse=true` across token boundaries. The highlighter's
    /// per-token RESET could not clear it, so subsequent token cells
    /// (Number, String, etc.) were baked with `style.reverse == true` —
    /// rendering as solid coloured blocks on Terminal.app (Terminal.app
    /// honours SGR state more strictly than iTerm2, which is why the
    /// bug was Terminal.app-specific even though the bytes were wrong
    /// everywhere).
    ///
    /// This test directly drives `parse_markdown_to_cells` with the
    /// exact pattern that triggered the leak: turn reverse ON, write
    /// some text, hit the highlighter's `RESET`, then write a tinted
    /// Number token. Post-fix (`RESET = "\x1b[0m"`), the Number cell
    /// must have `style.reverse == false`. Pre-fix, this assertion
    /// trips because reverse=true leaks past the partial RESET.
    #[test]
    fn markdown_stream_does_not_leak_reverse_across_tokens() {
        // Pattern: reverse-ON, "Y", theme::RESET, then a Number token
        // wrapped with truecolor open + theme::RESET close — mirrors how
        // an ApprovalPrompt chip's reverse state could leak into a
        // syntect-highlighted code block.
        let stream = format!(
            "\x1b[7mY{reset}\x1b[38;2;209;154;102m42{reset}",
            reset = crate::highlight::theme::RESET,
        );

        let lines = parse_markdown_to_cells(&stream);
        assert_eq!(lines.len(), 1, "expected single line, got {:?}", lines);
        let row = &lines[0];

        // Find the Number-token cells (chars '4' and '2').
        let number_cells: Vec<&Cell> =
            row.iter().filter(|c| c.ch == '4' || c.ch == '2').collect();
        assert_eq!(
            number_cells.len(),
            2,
            "expected the two Number-token cells, got row: {:?}",
            row,
        );

        for cell in &number_cells {
            assert!(
                !cell.style.reverse,
                "Number-token cell {:?} must NOT carry reverse=true — \
                 the highlighter's RESET must fully clear SGR state \
                 (including SGR 7 reverse) emitted upstream. If this \
                 fires, `theme::RESET` likely regressed back to a partial \
                 close like `\\x1b[23;39m` that only clears italic + fg.",
                cell,
            );
        }

        // Sanity: the leading "Y" cell DOES carry reverse — confirms the
        // SGR 7 actually took effect and the test isn't trivially green
        // because reverse never made it into the style stream.
        let y_cell = row
            .iter()
            .find(|c| c.ch == 'Y')
            .expect("expected the leading reverse-Y cell, got row: {:?}");
        assert!(
            y_cell.style.reverse,
            "control: the upstream `Y` cell must carry reverse=true so we \
             know the SGR 7 entered the style stream. If THIS fires, the \
             test setup is broken — not the fix.",
        );
    }

    /// Regression: end-of-turn burst over the overflow boundary used to
    /// duplicate every overflow-pushed row on the physical terminal.
    ///
    /// Real-world repro (user screenshot):
    ///   "以上就是 Java 语法高亮的完整速查……"   ← duplicated
    ///   "以上就是 Java 语法高亮的完整速查……"
    ///   "✓ Done · 1 轮 · 0 工具 · …"           ← duplicated
    ///   "✓ Done · 1 轮 · 0 工具 · …"
    ///
    /// Mechanism: `emit_body_line_inner` direct-wrote the new body row
    /// at row `footer_top_1idx = cap + 1` (one row BELOW where the body
    /// tail should actually live after the bottom-LF scroll shifted the
    /// footer up by 1). The follow-up `paint_frame` cell-diff then
    /// painted the same row at row `cap` (correct). The ghost write at
    /// `cap + 1` survived because successive overflow pushes shifted it
    /// up into the visible body region, and the cell-diff couldn't
    /// erase the ghost glyphs at columns where `prev_cells` (post
    /// `shift_prev_up`) and `cells` (post `paint_body_into_cells`)
    /// both held blanks — the bug class `850a8a47` flagged for blank-
    /// cell diff suppression, applied here to "stale glyph from earlier
    /// emit's wrong-row write" instead of "stale reverse-video bit".
    ///
    /// Test: simulate the burst (final assistant paragraph + 3-row
    /// TurnSeparator), confirm each marker text appears EXACTLY once
    /// in the visible body region. Pre-fix this asserts at 2x; post-
    /// fix it stays at 1x.
    #[test]
    fn retained_overflow_burst_does_not_duplicate_tail_rows() {
        let w: u16 = 80;
        let h: u16 = 12;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);

        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let cap = (h as usize).saturating_sub(r.current_footer_rows());

        // Fill body exactly to cap with filler so the upcoming burst
        // is guaranteed to ride the overflow branch every push.
        for i in 0..cap {
            r.render(UiLine::AssistantText(format!("filler {}\n", i)));
        }
        r.flush_deferred();
        buf.lock().unwrap().clear();

        // Burst: final paragraph chunk (line-terminated so it flushes
        // inside `flush_assistant_lines`) + TurnSeparator (which pushes
        // three rows: blank spacer / rule_with_label / blank spacer).
        // No `flush_deferred` between pushes so the cell-diff sees the
        // accumulated `shift_prev_up`/`invalidate_rows_from` state ALL
        // AT ONCE — exactly how a real end-of-turn burst arrives.
        r.render(UiLine::AssistantText("ENDPARA_UNIQUE\n".into()));
        r.render(UiLine::TurnSeparator {
            label: "DONE_LABEL_UNIQUE".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Count occurrences on the entire screen (body + footer). The
        // duplicate ghost lands in the body region, never on a footer
        // row — but we scan the whole screen so a regression that
        // displaces the dup elsewhere still trips this test.
        let count = |needle: &str| -> usize {
            (0..h as usize)
                .filter(|&row_0idx| vterm.row_text(row_0idx).contains(needle))
                .count()
        };
        let occ_para = count("ENDPARA_UNIQUE");
        let occ_label = count("DONE_LABEL_UNIQUE");
        assert_eq!(
            occ_para, 1,
            "ENDPARA_UNIQUE must appear EXACTLY once after end-of-turn \
             overflow burst (found {}). Pre-fix this was 2 — the \
             overflow direct-write landed at row cap+1 (one below the \
             body tail) and the follow-up cell-diff painted the same \
             row at cap; the ghost at cap+1 then got shifted up into \
             the visible body region by successive overflow pushes.\ndump:\n{}",
            occ_para,
            vterm.dump()
        );
        assert_eq!(
            occ_label, 1,
            "DONE_LABEL_UNIQUE must appear EXACTLY once after end-of-turn \
             overflow burst (found {}). See ENDPARA_UNIQUE assertion \
             above for the mechanism.\ndump:\n{}",
            occ_label,
            vterm.dump()
        );
    }

    /// Regression: `/resume` replay calls `reset()` then direct-writes
    /// historical rows via `emit_body_line_inner` BEFORE the next
    /// `paint_frame` tick. Pre-fix, `reset()` rebuilt `screen` with a
    /// fresh blank `prev_cells` AND blank `cells` but left
    /// `physical_dirty = false`, so the first post-reset render_diff
    /// skipped the cold-start CUP+EL preamble. Stale-column glyphs
    /// from the replay direct-writes then survived in the physical
    /// terminal — the cell-diff saw `prev_blank == new_blank` at
    /// trailing columns and emitted no patch — and the next streaming
    /// overflow burst ghosted those fragments into the visible body,
    /// producing duplicated tail rows ONLY after `/resume` (never
    /// after `/new`, whose `body_lines` starts empty so no replay
    /// direct-writes have populated the physical terminal).
    ///
    /// Fix: `reset()` ends with `screen.invalidate()` so the next
    /// `render_diff` begins with the cold-start preamble (per-row
    /// CUP+EL across every row) and wipes everything before patches
    /// land. From that point on `prev_cells` correctly mirrors
    /// physical and the existing db346457 overflow-target fix works
    /// for resumed sessions the same way it does for fresh ones.
    ///
    /// We test the contract at the byte-stream level: after `reset()`,
    /// the next `render_diff` MUST emit the per-row CUP+EL preamble.
    /// This mirrors `screen::tests::invalidate_prepends_per_row_clear_on_next_diff`
    /// but at the renderer boundary (the public reset API), so a
    /// future refactor that drops the `invalidate()` call here can't
    /// silently regress the contract by relying on Screen-internal
    /// invariants the renderer never enforced.
    #[test]
    fn retained_post_reset_replay_then_burst_does_not_duplicate_tail() {
        let w: u16 = 40;
        let h: u16 = 8;
        let (mut r, buf) = new_capturing(w, h);

        // Warm the renderer: push some body content + paint so
        // `prev_cells` is NOT all-blank when reset() runs. Without
        // this the bug is hidden — Screen::new() already starts with
        // blank prev_cells, so a "no invalidate" reset wouldn't show
        // any difference on the FIRST post-reset diff. Real-world
        // /resume always runs against a populated prev_cells (the
        // pre-resume session painted something), so the warm-up is
        // load-bearing for reproducing the bug class.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.render(UiLine::AssistantText("pre-resume content\n".into()));
        r.flush_deferred();
        buf.lock().unwrap().clear();

        // /resume hard reset. Pre-fix this rebuilt Screen with
        // `physical_dirty = false`; post-fix it ends with
        // `screen.invalidate()` so the next diff cold-starts.
        r.reset();

        // Simulate replay phase: push one body row + render the
        // prompt so paint_frame has something to draw. The exact
        // content doesn't matter for the cold-start assertion — we
        // just need to reach `flush_deferred → paint_frame →
        // render_diff` so the first post-reset diff actually runs.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.render(UiLine::AssistantText("replay row 0\n".into()));

        // Snapshot the buffer right before the first post-reset
        // paint so we can isolate render_diff's output (the
        // direct-write emits from `emit_body_line_inner` above also
        // live in `buf` but we want to assert only on what
        // render_diff produces — that's where the cold-start
        // preamble must appear).
        let pre_paint_len = buf.lock().unwrap().len();

        // First post-reset paint tick.
        r.flush_deferred();

        // Inspect the bytes emitted by the first post-reset
        // render_diff. Pre-fix, `physical_dirty` was false so
        // render_diff skipped its cold-start preamble and produced
        // ONLY cell-diff patches. Post-fix, every row must be
        // preceded by `CUP(row,1) + EL` somewhere in the output.
        let post_paint = {
            let g = buf.lock().unwrap();
            g[pre_paint_len..].to_vec()
        };
        let post_paint_str = String::from_utf8_lossy(&post_paint);

        // The cold-start preamble emits per-row CUP+EL for every row
        // from 1..=h. Asserting on all of them makes the test robust
        // against a partial regression (e.g. someone replacing the
        // loop with a `\x1b[2J` would fail this just like dropping
        // invalidate() entirely would).
        for row in 1..=(h as usize) {
            let needle = format!("\x1b[{};1H\x1b[K", row);
            assert!(
                post_paint_str.contains(&needle),
                "first render_diff after reset() must emit cold-start CUP+EL \
                 for row {} (pre-fix: physical_dirty=false → preamble skipped \
                 → stale replay direct-write glyphs survive → next overflow \
                 ghosts them into visible body as duplicated tail).\nbytes: {:?}",
                row,
                post_paint_str
            );
        }
    }

    /// Regression for the reasoning-text SGR-in-cells corruption: when
    /// `UiLine::ReasoningText` arrived, the handler wrapped the payload
    /// in `\x1b[2m...\x1b[0m` and routed it through `push_body_text` →
    /// `push_str_cells`, which has no SGR awareness. The 4 ESC/[/2/m
    /// bytes ended up as 4 width-1 cells, but the terminal consumes
    /// them as SGR without advancing the cursor — so cell index N+4
    /// landed at visual column N on screen, and subsequent cell-diff
    /// CUP patches addressed cells by model-column, overshooting their
    /// visual target by 4. Symptom: words like "Now let me start"
    /// rendered as "Now let letsmerstart", spaces became letters from
    /// the next flush's content.
    ///
    /// This test pins the invariant: no row produced by ReasoningText
    /// may contain the ESC byte (0x1b), `[`, `2`, `m`, or `0` as a
    /// trailing-SGR cell. The faint attribute belongs in `CellStyle`,
    /// not in the cell payload.
    #[test]
    fn retained_reasoning_text_does_not_embed_sgr_bytes_as_cells() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, _buf) = new_capturing(w, h);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.render(UiLine::ReasoningText(
            "Now let me start executing the 11 steps in order.\n".into(),
        ));
        r.flush_deferred();

        // Walk every body row's cells and assert none of them contain
        // the ESC byte as a glyph. Pre-fix the row built as
        // `[pad, pad, ESC, [, 2, m, N, o, w, ...]` would fail here on
        // the third cell (`ch == '\x1b'`).
        for (row_idx, row) in r.body_lines.iter().enumerate() {
            for (col_idx, cell) in row.iter().enumerate() {
                assert_ne!(
                    cell.ch, '\x1b',
                    "row {} col {}: ESC byte leaked into a cell — \
                     reasoning SGR is being injected as text instead of \
                     CellStyle.faint",
                    row_idx, col_idx
                );
            }
        }

        // Bonus check: the row that actually contains the reasoning
        // text should mark its content cells with `faint = true` so
        // the visual dim style still renders.
        let has_faint_text = r.body_lines.iter().any(|row| {
            row.iter().any(|c| c.style.faint && c.ch != ' ')
        });
        assert!(
            has_faint_text,
            "no faint reasoning cells found — the dim style was lost \
             when the SGR string approach was removed"
        );
    }

    /// Repro of the real-world `/whoami after big turn` corruption:
    /// when `current_footer_rows()` grows between two body pushes
    /// (slash menu opens as the user types `/whoami`, footer grows
    /// from 4→7 rows, cap shrinks 64→61), the next emit_body_line_inner
    /// finds `visible_len = 64 > cap = 61`. The pre-fix single-LF path
    /// only promoted ONE row to scrollback, so subsequent pushes
    /// computed `target_1idx = visible_len + 1 = 64` and direct-wrote
    /// body rows OVER the footer/prompt area. Visible artifact: the
    /// `cuizk@csdn.net` body row leaked into the user-echo line,
    /// rendering as `❯ /whoami@csdn.net` (per the BPUSH/BEMIT trace
    /// the user captured: `cap=61 visible_len=64 overflow=true` on
    /// every /whoami push, never converging).
    ///
    /// The catch-up `while visible_len >= cap` loop converges in one
    /// emit by LFing the exact excess (4 rows in this scenario).
    #[test]
    fn retained_footer_grow_then_push_catches_up_no_footer_overlap() {
        let w: u16 = 92;
        let h: u16 = 68; // mirrors the user's terminal geometry
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        let status = status_basic();

        // Establish footer4 (no menu): top_rule + input + bot_rule + status.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let cap_before = (h as usize).saturating_sub(r.current_footer_rows());

        // Fill body to exactly `cap_before` visible rows so visible_len = cap.
        for i in 0..(cap_before + 10) {
            r.render(UiLine::AssistantText(format!("filler-{:03}\n", i)));
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Now open the slash menu (3 items + extra footer rows) — same as
        // typing `/w` in the live binary. This grows footer rows WITHOUT
        // pushing any body row, so `scrolled_off` stays put while `cap`
        // shrinks. Mirrors the trace's footer4 → footer7 transition.
        let menu = Some(crate::render::MenuPayload {
            items: vec![
                ("whoami".into(), "Show current user".into()),
                ("wiki".into(), "Open wiki".into()),
                ("watch".into(), "Watch mode".into()),
            ],
            selected: 0,
            kind: crate::render::MenuKind::SlashCommand,
        });
        r.render(UiLine::InputPrompt {
            buf: "/w".into(),
            cursor_byte: 2,
            menu: menu.clone(),
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let cap_after = (h as usize).saturating_sub(r.current_footer_rows());
        assert!(
            cap_after < cap_before,
            "menu open did not shrink cap (cap_before={}, cap_after={})",
            cap_before,
            cap_after
        );

        // User hits Enter → /whoami runs. Menu stays in state at push
        // time (the InputPrompt that clears it doesn't fire until AFTER
        // CommandOutput; see the BPUSH trace). Each push must catch up
        // the (cap_before - cap_after) excess rows to scrollback so the
        // direct write never overshoots into the footer region.
        let whoami_text =
            "  TheoCui (saulcy)\n  cuizk@csdn.net\n  auth: /Users/theo/.atomcode/auth.toml\n";
        r.render(UiLine::User("/whoami".into()));
        r.render(UiLine::CommandOutput(whoami_text.into()));
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // The bug's signature: cells from the body's tail end up in the
        // footer region (rows >= cap). Scan the footer band for any
        // /whoami payload — if any whoami chars land there, the push
        // wrote past `cap`.
        let leak_check = |needle: &str| {
            // Footer occupies rows >= cap_after when the menu is open at
            // emit time. After /whoami's InputPrompt re-render the menu
            // clears, but the LEAKED cells (from direct writes done while
            // cap was 61) persist as ghost glyphs at those rows.
            (cap_after..h as usize).any(|row| vterm.row_text(row).contains(needle))
        };
        // The most diagnostic payload — the email — is what leaked in
        // the screenshot. Other whoami rows would also be a leak.
        assert!(
            !leak_check("@csdn.net"),
            "body row leaked into footer region (@csdn.net found in rows {}+).\n\nGrid:\n{}",
            cap_after,
            vterm.dump()
        );
        assert!(
            !leak_check("TheoCui"),
            "TheoCui row leaked into footer region.\n\nGrid:\n{}",
            vterm.dump()
        );
        assert!(
            !leak_check("auth:"),
            "auth: row leaked into footer region.\n\nGrid:\n{}",
            vterm.dump()
        );
    }

    /// Repro: user runs `/whoami` AFTER the body has already overflowed
    /// once (screen full from a previous large response). The command
    /// output is a single `UiLine::CommandOutput` whose payload is a
    /// 3-line `\n`-separated string ending in `\n`. `push_body_text_sgr`
    /// splits on `\n` which yields 4 chunks (3 content + 1 trailing
    /// empty), and each chunk goes through `push_body_row` →
    /// `emit_body_line_inner`. With body already at `cap`, every push
    /// triggers an overflow LF.
    ///
    /// Each of the 3 visible content lines must appear EXACTLY ONCE
    /// across (visible grid + scrollback). The user-reported screenshot
    /// showed e.g. `auth: ...` twice and `TheoCui (saulcy)` rendered in
    /// the wrong slot — this test pins that invariant.
    #[test]
    fn retained_whoami_after_screen_fill_renders_each_line_exactly_once() {
        let w: u16 = 67;
        let h: u16 = 41;
        let (mut r, buf) = new_capturing(w, h);
        let mut vterm = crate::test_term::VirtualTerminal::new(w, h);
        let status = status_basic();

        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let cap = (h as usize).saturating_sub(r.current_footer_rows());

        // Fill body well past cap so several overflow LFs have already
        // happened — this is what makes `scrolled_off > 0` when the
        // whoami output starts streaming. Interleave a spinner tick
        // (mimics the real session's StreamingBox + Spinner cadence).
        for i in 0..(cap + 10) {
            r.render(UiLine::AssistantText(format!("filler-bullet-{:03}\n", i)));
            if i % 4 == 0 {
                r.render(UiLine::Spinner {
                    frame: "⠋".into(),
                    label: "Pondering…".into(),
                });
            }
        }
        // Mimic real turn-end: line-break + TurnSeparator. The separator
        // pushes 3 rows (blank, rule, blank) which themselves may cause
        // additional overflow LFs.
        r.render(UiLine::AssistantLineBreak);
        r.render(UiLine::TurnSeparator {
            label: "✓ Done · 5 轮 · 6 工具 · 26.4s · 1696 tokens".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // User types `/whoami`. The event loop echoes the user input
        // BEFORE running the command — render that too so the body
        // state matches the real screenshot.
        let whoami_text =
            "  TheoCui (saulcy)\n  cuizk@csdn.net\n  auth: /Users/theo/.atomcode/auth.toml\n";
        r.render(UiLine::User("/whoami".into()));
        r.render(UiLine::CommandOutput(whoami_text.into()));
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Second invocation — mimics the screenshot showing the bug
        // surfaced across BOTH /whoami runs.
        r.render(UiLine::User("/whoami".into()));
        r.render(UiLine::CommandOutput(whoami_text.into()));
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let lines = [
            "TheoCui (saulcy)",
            "cuizk@csdn.net",
            "auth: /Users/theo/.atomcode/auth.toml",
        ];
        for line in lines {
            let scrollback_count = vterm
                .scrollback_texts()
                .iter()
                .filter(|row| row.contains(line))
                .count();
            let visible_count = (0..h as usize)
                .filter(|i| vterm.row_text(*i).contains(line))
                .count();
            let total = scrollback_count + visible_count;
            // Two /whoami invocations × 1 occurrence per line = 2.
            assert_eq!(
                total, 2,
                "line {:?} should appear exactly once after /whoami \
                 (visible={}, scrollback={})\n\nvisible grid:\n{}\n\n\
                 scrollback tail:\n{}",
                line,
                visible_count,
                scrollback_count,
                vterm.dump(),
                vterm
                    .scrollback_texts()
                    .iter()
                    .rev()
                    .take(20)
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
}
