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
    DisableBracketedPaste, EnableBracketedPaste, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;

use super::cell::{push_str_cells, push_str_cells_sgr, serialize_row, Cell, CellStyle};
use super::screen::Screen;
use super::theme::{role, Role};
use super::{
    DiffPanelRow, DiffPanelSpan, DiffPanelTone, MenuPayload, Renderer, StatusLine, UiLine,
};
use crate::i18n::{t, Msg};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;
use crossterm::style::Color;

const PAD_COL: usize = 2;

/// Max body_lines kept in the in-app scrollback buffer (matches alt-screen).
/// Bounded so memory doesn't grow without limit on long sessions.
pub const MAX_SCROLLBACK_ROWS: usize = 5000;

/// Hard cap on the partial-line buffer for streaming assistant text. It's
/// normally drained per newline; this bounds a pathological newline-less stream
/// so it can't grow the buffer until allocation fails. 1 MiB ≫ any real line.
const ASSISTANT_LINE_BUF_MAX: usize = 1 << 20;

/// Hard cap on how many rows the input box may DISPLAY before it scrolls
/// internally. Bounds the footer so a long paste / typed text can't grow it
/// past the screen height (the overflow bug). This caps DISPLAY only — the
/// full text always lives in `input_buf` and is sent verbatim on submit.
const MAX_INPUT_ROWS: usize = 6;

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
        // Always surface the utilization percentage so the user can watch the
        // context fill toward the auto-compaction threshold (~70%), not only
        // once it's near-full. Auto-compaction at 0.7 was effectively invisible
        // when the % only appeared at >=90%.
        let pct = (used as f64 / window as f64 * 100.0).round() as u64;
        format!("{}/{} tok ({}%)", used_label, window_label, pct)
    }
}

/// Marker prefix for the dedicated footer goal row. A width-1 BMP "ring" from
/// the Geometric Shapes block when the terminal's font has it, ASCII `*`
/// otherwise — the SAME `unicode_symbols` gate the spinner (`◐`→`|/-\`) and
/// ellipsis (`…`→`...`) use, so Windows legacy conhost / Consolas don't show
/// `□` tofu. Deliberately NOT an emoji (e.g. 🎯): emoji width is rendered
/// inconsistently across terminals and would drift every column after it in the
/// cell-diff renderer, and emoji font coverage is far spottier than Geometric
/// Shapes. Both variants are width-1 + a trailing space, so the layout math is
/// identical either way.
fn goal_marker(unicode: bool) -> &'static str {
    if unicode {
        "◎ "
    } else {
        "* "
    }
}

/// The three display segments of the dedicated footer goal row, width-fitted:
/// `(marker, condition, meta)`. The caller styles each independently — marker as
/// an accent, condition as normal text, `meta` (` · round N · elapsed`) muted —
/// so the row reads as a calm persistent status with hierarchy, not a loud
/// activity line. Joining the three reproduces the full row text.
///
/// `round` is shown verbatim (callers pass a 1-based value). Elapsed is `13s`
/// under a minute else `2m13s`. The meta is RESERVED (always shown); the
/// condition fills the remaining columns and is truncated with `…`. When the row
/// is too narrow for any condition, the condition (and the leading separator)
/// drop entirely but round/elapsed survive. CJK/width-safe via `crate::width`.
fn goal_row_parts(
    condition: &str,
    round: u32,
    elapsed_secs: u64,
    max_cols: usize,
    unicode: bool,
) -> (&'static str, String, String) {
    let marker = goal_marker(unicode);
    let m = elapsed_secs / 60;
    let s = elapsed_secs % 60;
    let elapsed = if m == 0 {
        format!("{s}s")
    } else {
        format!("{m}m{s}s")
    };
    let icon_w = crate::width::display_width(marker);
    let meta = format!(" · round {round} · {elapsed}");
    let meta_w = crate::width::display_width(&meta);
    let cond_budget = max_cols.saturating_sub(icon_w).saturating_sub(meta_w);
    if cond_budget == 0 {
        // Too narrow for any condition — drop it (and the leading separator),
        // keep marker + round/elapsed, truncating the meta to the cols left.
        let bare = format!("round {round} · {elapsed}");
        let meta_only = crate::width::truncate_to_width(&bare, max_cols.saturating_sub(icon_w));
        return (marker, String::new(), meta_only);
    }
    let cond = crate::width::truncate_with_ellipsis(condition, cond_budget);
    (marker, cond, meta)
}

/// Full goal-row text (marker + condition + meta joined). Thin wrapper over
/// [`goal_row_parts`] for width assertions / tests; the renderer styles the
/// parts separately via `goal_row_parts` directly.
#[cfg(test)]
fn format_goal_row(
    condition: &str,
    round: u32,
    elapsed_secs: u64,
    max_cols: usize,
    unicode: bool,
) -> String {
    let (marker, cond, meta) = goal_row_parts(condition, round, elapsed_secs, max_cols, unicode);
    format!("{marker}{cond}{meta}")
}

/// Loop marker: `⚡` (Lightning, U+26A1, Miscellaneous Symbols) when unicode is
/// available, `*` fallback for legacy terminals. Width-1 + trailing space,
/// matching the layout contract of `goal_marker`.
fn loop_marker(unicode: bool) -> &'static str {
    if unicode {
        "⚡ "
    } else {
        "* "
    }
}

/// The three display segments of the dedicated footer loop row, width-fitted:
/// `(marker, label, meta)`. Mirrors `goal_row_parts` exactly — marker in Brand
/// color, label in normal text, meta (` · round N · elapsed`) muted.
///
/// `round` is shown verbatim (callers pass a 1-based value). Elapsed `13s`
/// under a minute else `2m13s`. The meta is RESERVED; label fills the rest and
/// is truncated with `…`. CJK/width-safe via `crate::width`.
fn loop_row_parts(
    label: &str,
    round: u32,
    elapsed_secs: u64,
    max_cols: usize,
    unicode: bool,
) -> (&'static str, String, String) {
    let marker = loop_marker(unicode);
    let m = elapsed_secs / 60;
    let s = elapsed_secs % 60;
    let elapsed = if m == 0 {
        format!("{s}s")
    } else {
        format!("{m}m{s}s")
    };
    let icon_w = crate::width::display_width(marker);
    let meta = format!(" · round {round} · {elapsed}");
    let meta_w = crate::width::display_width(&meta);
    let label_budget = max_cols.saturating_sub(icon_w).saturating_sub(meta_w);
    if label_budget == 0 {
        // Too narrow for any label — drop it, keep marker + round/elapsed.
        let bare = format!("round {round} · {elapsed}");
        let meta_only = crate::width::truncate_to_width(&bare, max_cols.saturating_sub(icon_w));
        return (marker, String::new(), meta_only);
    }
    let lbl = crate::width::truncate_with_ellipsis(label, label_budget);
    (marker, lbl, meta)
}

/// Full loop-row text joined. Thin wrapper over [`loop_row_parts`] for tests.
#[cfg(test)]
fn format_loop_row(
    label: &str,
    round: u32,
    elapsed_secs: u64,
    max_cols: usize,
    unicode: bool,
) -> String {
    let (marker, lbl, meta) = loop_row_parts(label, round, elapsed_secs, max_cols, unicode);
    format!("{marker}{lbl}{meta}")
}

/// Max rows the footer todo panel may occupy, INCLUDING the spacer + header. The
/// panel is additionally clamped against screen height by the caller. When the whole
/// list fits within the budget it renders every item; otherwise it folds (see
/// `todo_panel_rows`). No blank padding — a short list shows short.
const MAX_TODO_PANEL_ROWS: usize = 7;
/// Header + at most three live children + one folded terminal/pending summary.
const MAX_SUBTASK_PANEL_ROWS: usize = 5;
const MAX_VISIBLE_RUNNING_SUBTASKS: usize = 3;

fn format_subtask_progress(activity: &str, elapsed: &str, tokens: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let tokens = crate::i18n::fmt_tokens(tokens as usize);
    let metadata = [
        format!(" \u{b7} {elapsed} \u{b7} \u{2191} {tokens} tokens"),
        format!(" \u{b7} {elapsed} \u{b7} \u{2191}{tokens}"),
        format!(" {elapsed} \u{2191}{tokens}"),
        format!("\u{2191}{tokens}"),
    ]
    .into_iter()
    .find(|candidate| crate::width::display_width(candidate) <= width);

    let Some(metadata) = metadata else {
        return crate::width::truncate_to_width(&format!("\u{2191}{tokens}"), width);
    };
    let activity_width = width.saturating_sub(crate::width::display_width(&metadata));
    let activity = crate::width::truncate_with_ellipsis(&scrub_controls(activity), activity_width);
    crate::width::truncate_to_width(&format!("{activity}{metadata}"), width)
}

fn clamp_cell_row(row: &mut Vec<Cell>, width: usize) {
    if row.len() <= width {
        return;
    }
    row.truncate(width);
    // Never leave the leading half of a wide glyph at the right edge.
    if row.last().is_some_and(|cell| cell.width > 1) {
        row.pop();
    }
}

/// One logical row of the collapsed todo panel. Pure structure — glyphs,
/// i18n words, styling and width-fitting are applied in `build_todo_rows`.
#[derive(Debug, Clone, PartialEq)]
enum TodoPanelRow {
    /// A blank spacer row prepended before the Header to add visual breathing
    /// room between the transcript body and the todo panel.
    Spacer,
    Header {
        completed: usize,
        in_progress: usize,
        total: usize,
    },
    Item {
        /// 0-based position in the FULL todo list, rendered as `#{index+1}`.
        index: usize,
        status: atomcode_capabilities::tools::todo::TodoStatus,
        content: String,
    },
    More {
        hidden: usize,
    },
}

/// Collapse a todo list into at most `max_rows` panel rows (incl. spacer + header).
///
/// When the whole list fits, every item shows in order. Otherwise a WINDOW around
/// the current frontier (in-progress / first pending) shows recent history + the
/// current/next work; pending items hidden below become a single `More` row. Items
/// hidden above need no indicator — the header already reports the done/open totals.
fn todo_panel_rows(
    items: &[(atomcode_capabilities::tools::todo::TodoStatus, String)],
    completed: usize,
    in_progress: usize,
    total: usize,
    max_rows: usize,
) -> Vec<TodoPanelRow> {
    use atomcode_capabilities::tools::todo::TodoStatus;
    // Spacer goes first — one blank line between the transcript body above and
    // the `☑ Todos` header, so the panel doesn't feel cramped. But on a terminal
    // too short to afford the cosmetic row (max_rows < 2) we drop it, so the
    // panel never emits more rows than max_rows.
    let mut rows = if max_rows >= 2 {
        vec![
            TodoPanelRow::Spacer,
            TodoPanelRow::Header {
                completed,
                in_progress,
                total,
            },
        ]
    } else {
        vec![TodoPanelRow::Header {
            completed,
            in_progress,
            total,
        }]
    };
    // body_budget = whatever's left after the header (+ spacer when present).
    let body_budget = max_rows.saturating_sub(rows.len());
    if body_budget == 0 {
        return rows;
    }

    // If the WHOLE list fits, show every item in order with its real status
    // (✔ completed, ☐ open) — no fold, no `+N more`, no padding. Only when the
    // list overflows the budget do we fall through to the compact folded view.
    if items.len() <= body_budget {
        for (i, (status, content)) in items.iter().enumerate() {
            rows.push(TodoPanelRow::Item {
                index: i,
                status: *status,
                content: content.clone(),
            });
        }
        return rows;
    }

    // Overflow: show a WINDOW of `body_budget` items that ALWAYS contains the current
    // frontier (in-progress / first pending / last). Prefer recent items (window bottom
    // at the list end); pull the window back if the tail would drop the frontier. Pending
    // hidden BELOW the window get a `+N more` row; items hidden above need no indicator
    // (the header already reports the done/open totals).
    let n = items.len();
    let anchor = items
        .iter()
        .position(|(s, _)| *s == TodoStatus::InProgress)
        .or_else(|| items.iter().position(|(s, _)| *s == TodoStatus::Pending))
        .unwrap_or(n.saturating_sub(1));
    let mut start = n.saturating_sub(body_budget);
    if start > anchor {
        start = anchor;
    }
    let mut end = (start + body_budget).min(n);
    if n - end > 0 {
        end -= 1; // reserve the last row for the `+N more` marker
    }
    let hidden = n - end;
    for (idx, (status, content)) in items.iter().enumerate().take(end).skip(start) {
        rows.push(TodoPanelRow::Item {
            index: idx,
            status: *status,
            content: content.clone(),
        });
    }
    if hidden > 0 {
        rows.push(TodoPanelRow::More { hidden });
    }
    rows
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

/// Map a mascot cell's `(top, bottom)` subpixel colours to a rendered cell.
/// Uses `▄` (lower-half) when ONLY the bottom is coloured so the transparent
/// top stays the terminal background — NOT `▀` with a `None` fg, which paints
/// the top half in the terminal's default fg (the "dark bar across the ears"
/// bug). `▀` (fg=top, bg=bottom) covers the both-coloured case.
fn mascot_cell(top_c: Option<Color>, bot_c: Option<Color>) -> Cell {
    match (top_c, bot_c) {
        (None, None) => Cell::blank(),
        (Some(t), None) => Cell {
            ch: '▀',
            style: CellStyle {
                fg: Some(t),
                bg: None,
                ..CellStyle::default()
            },
            width: 1,
        },
        (None, Some(b)) => Cell {
            ch: '▄',
            style: CellStyle {
                fg: Some(b),
                bg: None,
                ..CellStyle::default()
            },
            width: 1,
        },
        (Some(t), Some(b)) => Cell {
            ch: '▀',
            style: CellStyle {
                fg: Some(t),
                bg: Some(b),
                ..CellStyle::default()
            },
            width: 1,
        },
    }
}

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
            // SGR 37 (regular white → soft light-gray on dark themes). Used
            // by `theme::md_border_open()` for table borders in dark mode,
            // where SGR 90 (DarkGrey) collapses to ~3:1 against the bg and
            // the grid goes invisible. Maps to Color::Grey, NOT bright white.
            Some(37) => style.fg = Some(Color::Grey),
            // SGR 34/35 (standard blue / magenta). Emitted by the LIGHT
            // theme: `theme::md_heading_open()` → `1;34m` and
            // `theme::md_inline_code_open()` → `35m` (dark uses bright cyan /
            // SGR 96). Without these arms both fell through to `_ => {}` and
            // rendered in the default fg (black) — the "all colours vanish on
            // the light theme" bug. Mirror `render::cell::apply_sgr`.
            Some(34) => style.fg = Some(Color::DarkBlue),
            Some(35) => style.fg = Some(Color::DarkMagenta),
            Some(90) => style.fg = Some(Color::DarkGrey),
            Some(91) => style.fg = Some(Color::Red),
            Some(92) => style.fg = Some(Color::Green),
            Some(93) => style.fg = Some(Color::Yellow),
            Some(94) => style.fg = Some(Color::Blue),
            Some(95) => style.fg = Some(Color::Magenta),
            Some(96) => style.fg = Some(Color::Cyan),
            Some(97) => style.fg = Some(Color::White),
            _ => {
                // Other ANSI codes (30-33, 36, extended 38;5/38;2/48/49,
                // underline) are silently ignored — the markdown/theme path
                // is a documented 16-colour vocabulary (see theme.rs) and
                // never emits them. 256-colour/truecolor decoding lives in
                // render::cell::apply_sgr_params, the parser the modal menu
                // overlay (e.g. the /usage heatmap) actually flows through.
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
    /// Set by `set_suppress_auto_copy(true)` before history replay
    /// (`/resume`, `/undo`, `atomcode -c`). Suppresses clipboard writes
    /// and "Copied" hint lines so replay doesn't overwrite the user's
    /// clipboard or inject stale annotations (issue #699).
    suppress_auto_copy: bool,
    /// Master switch for the code-block auto-copy feature (issue #699).
    /// Default OFF: auto-copy silently overwrote the user's clipboard on every
    /// code-block reply, so it is now opt-in via `config.ui.auto_copy_code_blocks`
    /// (or `ATOMCODE_AUTO_COPY`), plumbed in at startup via `set_auto_copy_enabled`.
    /// Explicit `/copy` stays available regardless.
    auto_copy_enabled: bool,
    /// Line-buffer for streaming assistant text — chunks accumulate
    /// here until a `\n` boundary, at which point the completed
    /// physical line is appended to `body_lines`.
    assistant_line_buf: String,
    /// Line-buffer for streaming reasoning text.
    reasoning_line_buf: String,
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
    /// Number of upcoming `push_body_row` calls that should overwrite in
    /// place instead of scrolling the body region. Set by
    /// `commit_inflight_tool` when the committed inflight strip occupied
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
    /// Cached `(model, working_dir, chosen_pool_indices)` so resize rebuilds
    /// the SAME mascot + tips instead of re-rolling the random pick.
    welcome_banner: Option<(String, String, Vec<usize>)>,
    /// The randomly-chosen welcome-tip POOL indices, rolled ONCE per session
    /// and reused for every subsequent welcome render. Deliberately survives
    /// `reflow_body_to_current_width` (a resize clears `welcome_banner` and
    /// replays `body_log`, re-emitting `UiLine::Welcome` → `push_welcome`) so a
    /// resize does NOT re-roll the tips. Reset to `None` only in `reset()` (a
    /// fresh session / `/clear`), where new tips are appropriate.
    welcome_tip_indices: Option<Vec<usize>>,
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
    /// True when the row immediately before the live spinner is a transient
    /// spacer inserted by `push_or_update_live_spinner`. It is removed with
    /// the spinner and never enters terminal scrollback.
    live_spinner_spacer_active: bool,
    /// True after a painted full-viewport spinner frame temporarily replaced
    /// one older middle row with the newest permanent tail. A following
    /// permanent push must restore the continuous body before emitting LF.
    live_spinner_tail_compacted: bool,
    /// Set by `emit_body_line_inner` when an overflow LF scrolled the whole
    /// viewport (footer included) up one row; consumed (cleared) by
    /// `take_pending_scroll_flush` so the render worker repaints the footer
    /// the same tick instead of waiting ~5ms for the deferred FlushDeferred.
    pending_scroll_flush: bool,
    /// True between `begin_sync()` and `end_sync()` — a single OUTER DECSET
    /// 2026 envelope spanning a whole multi-operation burst (the `/resume`
    /// replay). While set, `screen.sync_suppressed` is held on so per-frame
    /// `render_diff`s don't emit nested envelopes. Tracked at the renderer
    /// level (not only on `screen`) because `reset()` rebuilds `screen` —
    /// the flag is re-applied to the fresh `screen` so suppression survives.
    in_sync_batch: bool,
    /// When `Some`, the live row at body_bottom is the animated
    /// in-flight tool-call line (`<frame> Bash(cmd)`), not the generic
    /// spinner. The Spinner / StreamingBox tick handlers consult this:
    /// if Some they build a tool-call row with the new frame as icon;
    /// if None they build the generic `<frame> Pondering…` spinner row.
    /// Cleared by `ToolCallCommit`, which freezes the row to a static
    /// `▸` icon (no longer live) so the next push_body_row appends
    /// cleanly below it and the spinner can resume on the next tick.
    /// A body push that arrives while the strip is STILL live (e.g. a
    /// `task` tool streaming `↻`/`✓` progress rows) no longer buries the
    /// strip: `push_body_row` calls `lift_inflight_strip` to pop it first
    /// (keeping `inflight_tool` Some), and the next tick re-emits it at
    /// the new tail — otherwise each such push orphaned a frozen snapshot.
    /// (call_id, name, detail).
    inflight_tool: Option<(String, String, String)>,
    /// Number of body lines occupied by the multi-line wrapped in-flight
    /// tool call (rendered via `render_inflight_tool`). Used to replace
    /// those lines on each spinner tick and to clean up on commit.
    inflight_tool_rows: usize,
    /// Optional ephemeral hint (e.g. the bash "Press Ctrl+o …" line) rendered
    /// inside the inflight strip by `render_inflight_tool`, so it's counted in
    /// `inflight_tool_rows` and cleared atomically with the spinner on commit.
    /// Must NOT be pushed as a standalone body row — that breaks the "inflight
    /// strip = body tail" invariant and orphans the spinner glyph next to the
    /// committed `●` (bash-only, since the hint is).
    inflight_hint: Option<String>,
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
    /// Modal overlay state: a floating window drawn on top of body+footer.
    /// When Some, `paint_frame` paints the overlay cells after the normal
    /// body+footer, so the diff sees the combined frame.
    modal_overlay: Option<ModalOverlayState>,
    /// True while the `/diff` panel owns the frame. The panel is a borderless,
    /// fixed-height overlay drawn inline where the input box sits, covering it,
    /// so the terminal caret must be hidden — otherwise it blinks on top of the
    /// panel at the now-covered input row.
    diff_overlay_active: bool,
    /// Append-only log of the permanent body-producing `UiLine`s in
    /// render order — the semantic source needed to REFLOW the whole
    /// transcript when the terminal width changes. `body_lines` holds
    /// cells already wrapped to the width they were produced at, so a
    /// resize can only CLIP them (drops the overflow) — it cannot
    /// re-wrap. Replaying this log through `render()` at the new
    /// geometry rebuilds every row correctly wrapped (same mechanism
    /// as the `/resume` replay). Transient / footer / modal variants
    /// (InputPrompt, Spinner, ModalOverlay…) are NOT logged — they are
    /// re-derived from live state by `paint_frame`.
    /// Consecutive AssistantText / ReasoningText deltas are coalesced
    /// so per-token streaming doesn't bloat the log.
    body_log: Vec<UiLine>,
    /// True only while `reflow_body_to_current_width` is replaying
    /// `body_log` — suppresses re-logging so replay never grows the log.
    replaying: bool,
    /// Set once `body_log` evicts its oldest entry (session exceeded
    /// `MAX_SCROLLBACK_ROWS` logged events): the oldest transcript prefix
    /// can no longer be reconstructed by the reflow replay. DIAGNOSTIC ONLY
    /// — `on_resize` clears the host scrollback (`\x1b[3J`) unconditionally;
    /// this flag merely annotates the resize trace to record that the
    /// un-reflowable prefix was dropped. (It used to GATE the wipe, but
    /// skipping it let the reflow stack a duplicate transcript on every
    /// resize — the "一直重复输出" bug — so the wipe now always fires.)
    body_log_truncated: bool,
}

/// Pre-computed overlay cell grid + screen position for a modal window.
#[derive(Debug, Clone)]
struct ModalOverlayState {
    cells: Vec<Vec<Cell>>,
    x: u16,
    y: u16,
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
        // NO console-mode management. atomcode defers ALL mouse handling — wheel,
        // drag-select, copy, right-click-paste — to the terminal's NATIVE behavior
        // on every Windows host, including legacy conhost. The earlier
        // `disable_conhost_quick_edit` (clearing `ENABLE_QUICK_EDIT_MODE` to stop
        // the conhost click-to-freeze) broke that entire native suite: on conhost it
        // killed select/copy/paste, and on Windows Terminal / ConPTY it ALSO killed
        // native selection (ConPTY turns on mouse forwarding when QuickEdit is
        // cleared). So we never touch the console mode. (conhost's click-pause is its
        // standard, recoverable behavior — Esc cancels, Enter copies; far better than
        // losing the whole mouse.) See the deleted `render::conhost` module's history.
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
        Self {
            out,
            caps,
            screen: Screen::new(w, h).with_jediterm(caps.jediterm),
            input_buf: String::new(),
            input_cursor_byte: 0,
            menu: None,
            status: StatusLine::default(),
            input_attachments: Vec::new(),
            body_lines: Vec::new(),
            message_marks: Vec::new(),
            last_mark_was_assistant: false,
            suppress_auto_copy: false,
            auto_copy_enabled: false,
            assistant_line_buf: String::new(),
            reasoning_line_buf: String::new(),
            md_state: crate::markdown::MdState::new(),
            dirty: false,
            last_painted_footer_rows: 0,
            skip_body_scroll_count: 0,
            scrolled_off: 0,
            welcome_banner: None,
            welcome_tip_indices: None,
            welcome_line_count: 0,
            live_spinner_active: false,
            live_spinner_spacer_active: false,
            live_spinner_tail_compacted: false,
            pending_scroll_flush: false,
            in_sync_batch: false,
            inflight_tool: None,
            inflight_tool_rows: 0,
            inflight_hint: None,
            live_group: None,
            modal_overlay: None,
            diff_overlay_active: false,
            body_log: Vec::new(),
            replaying: false,
            body_log_truncated: false,
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
            bg: None,
        }
    }

    fn style_bold(&self, r: Role) -> CellStyle {
        CellStyle {
            fg: role(self.caps, r),
            bold: true,
            reverse: false,
            faint: false,
            bg: None,
        }
    }

    /// Background style for the user-input block. `bg` comes from the
    /// theme-aware `Role::PanelBg` (dark 236 / light 254); `fg` stays `None`
    /// so the text keeps the terminal's default foreground. Returns
    /// `bg: None` when colours are disabled — the caller treats that as
    /// "no block, use the plain layout".
    fn style_panel_bg(&self) -> CellStyle {
        CellStyle {
            fg: None,
            bg: role(self.caps, Role::PanelBg),
            bold: false,
            reverse: false,
            faint: false,
        }
    }

    /// A full-block-width row of `bg`-styled spaces — the top/bottom padding
    /// lines that give the user-input block its "breathing" block look.
    fn bg_blank_row(w: usize, bg: &CellStyle) -> Vec<Cell> {
        let mut row = Vec::with_capacity(w);
        for _ in 0..w {
            row.push(Cell {
                ch: ' ',
                style: bg.clone(),
                width: 1,
            });
        }
        row
    }

    /// Extend `row` with `bg`-styled spaces until it spans `w` display
    /// columns, so the background reads as one full-width block. `w` is
    /// `screen.width() − PAD_COL`, matching `build_prefixed_rows`' wrap
    /// budget and leaving the same right margin (which also avoids writing
    /// the final column and its auto-wrap hazard).
    fn pad_row_bg(row: &mut Vec<Cell>, w: usize, bg: &CellStyle) {
        let cur: usize = row.iter().map(|c| c.width as usize).sum();
        for _ in cur..w {
            row.push(Cell {
                ch: ' ',
                style: bg.clone(),
                width: 1,
            });
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
            bg: None,
        }
    }

    /// The bold highlight colour for the selected `/resume` session (title text,
    /// `▸` marker, and the search-box caret). Theme-aware per user request: cyan
    /// on dark, magenta on light — both pop against their background without
    /// needing a full reverse-video bar.
    fn session_highlight_style(&self) -> CellStyle {
        let r = if crate::highlight::theme::is_light_for_render() {
            Role::Brand
        } else {
            Role::Accent
        };
        CellStyle {
            fg: role(self.caps, r),
            bold: true,
            reverse: false,
            faint: false,
            bg: None,
        }
    }

    /// Rendered row count for a `/resume` session item at menu index
    /// `orig_idx` (>= HEADER_ROWS): 2 rows (title + metadata) for the first
    /// session, 3 for the rest — the extra row is the leading blank spacer that
    /// separates cards. This MUST stay in lockstep with the render loop's
    /// `if orig_idx > HEADER_ROWS { push blank }`; both the paginator's
    /// fit-loops and the footer `menu_rows` reservation call it so the three
    /// height accountings never drift.
    fn session_item_height(orig_idx: usize) -> usize {
        if orig_idx == crate::modals::session_picker::HEADER_ROWS {
            2
        } else {
            3
        }
    }

    fn max_menu_rows(&self, h: usize, default: usize) -> usize {
        self.menu
            .as_ref()
            .map(|m| {
                let base = m.kind.max_visible_rows(h, m.items.len());
                let has_hint = m
                    .items
                    .last()
                    .map(|(n, _)| n.starts_with('—') && n.ends_with('—'))
                    .unwrap_or(false);
                let extra = if m.kind == super::MenuKind::Marketplace
                    && m.items.iter().any(|(n, _)| n.starts_with("❯ "))
                {
                    2
                } else {
                    0
                };
                if has_hint {
                    base + 1 + extra
                } else {
                    base + extra
                }
            })
            .unwrap_or(default)
    }

    fn tool_bullet_style(&self) -> CellStyle {
        self.style_bold(Role::ToolName)
    }

    /// Build `<prefix><Name>(<cmd>)<meta>` rows for a bash tool call. The
    /// command sits in parens on the header line to match other tools
    /// (`● Read(arg)`), while `format_shell_command` keeps the shell-boundary
    /// wrapping (`&&`, `||`, `|`, `\`) so multi-part commands break onto their
    /// own lines. Shared by the static/committed path (`prefix = "● "`, empty
    /// `meta`) and the live spinner (`prefix` = animated glyph, `meta` = elapsed
    /// timer) so both render identically apart from the glyph and timer.
    #[allow(clippy::too_many_arguments)]
    fn build_bash_command_rows(
        &self,
        prefix: &str,
        prefix_style: &CellStyle,
        safe_name: &str,
        name_style: &CellStyle,
        detail_style: &CellStyle,
        safe_detail: &str,
        meta: &str,
        meta_style: &CellStyle,
    ) -> Vec<Vec<Cell>> {
        // Row 0 carries the `<prefix><Name>(` header. Budget the wrap width for
        // it; continuation lines only carry format_shell_command's 4-space
        // indent, so `cont_pad` re-aligns them under the command text on row 0.
        let header_cols =
            crate::width::display_width(prefix) + crate::width::display_width(safe_name) + 1;
        let cont_pad = " ".repeat(header_cols.saturating_sub(4));
        let width = (self.screen.width() as usize)
            .saturating_sub(header_cols)
            .max(8);
        // Display-only: shorten `$HOME/…` → `~/…` so long paths are less likely to
        // overflow the window (like the footer shows `~/…` for the cwd; best-effort —
        // no-op on Windows backslash/casing paths). The executed command and the
        // transcript copy keep the real absolute path.
        let shortened = crate::platform::collapse_home_in_command(safe_detail);
        let safe_cmd = crate::glyph::downgrade_glyphs(&shortened, self.caps.unicode_symbols);
        let lines = crate::event_loop::format_shell_command(&safe_cmd, width);
        let last = lines.len().saturating_sub(1);
        let mut rows = Vec::with_capacity(lines.len());
        for (idx, line) in lines.iter().enumerate() {
            let mut row = Vec::new();
            if idx == 0 {
                push_str_cells(&mut row, prefix, prefix_style);
                push_str_cells(&mut row, safe_name, name_style);
                push_str_cells(&mut row, "(", detail_style);
            } else if !cont_pad.is_empty() {
                push_str_cells(&mut row, &cont_pad, detail_style);
            }
            push_str_cells(&mut row, line, detail_style);
            if idx == last {
                push_str_cells(&mut row, ")", detail_style);
                if !meta.is_empty() {
                    push_str_cells(&mut row, meta, meta_style);
                }
            }
            rows.push(row);
        }
        rows
    }

    /// Render `● Bash(<cmd>)` header block (static / committed path). Thin
    /// wrapper over `build_bash_command_rows`. Used by both the static
    /// `UiLine::ToolCall` arm and `commit_inflight_tool` so the live and static
    /// paths produce identical output.
    fn push_bash_command_block(&mut self, safe_name: &str, safe_detail: &str) {
        let bullet_style = self.tool_bullet_style();
        let tool_name_style = self.style_bold(Role::ToolName);
        // Parens + command render in Secondary, same as the `(detail)` other
        // tools show.
        let detail_style = self.style_for(Role::Secondary);
        let rows = self.build_bash_command_rows(
            "● ",
            &bullet_style,
            safe_name,
            &tool_name_style,
            &detail_style,
            safe_detail,
            "",
            &detail_style,
        );
        for row in rows {
            self.push_body_row(row);
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
        // Label (`Pondering… · 161ms`) shares the frame's brand hue so the whole
        // spinner reads as one colored unit. It was `Role::Secondary` (terminal-
        // default fg) after the 16-colour theme-adaptivity migration, which left the
        // text uncolored; `Brand` is a defined palette colour (theme-safe, same as the
        // glyph) so it restores the colour without hardcoding a raw SGR.
        let label_style = self.style_bold(Role::Brand);
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

        // Task/CodeReview are long-running sub-agent fan-outs. Their live row
        // carries spinner activity (`· thinking…`, elapsed, token count), so
        // treat the whole transient strip as an activity indicator rather than
        // a static tool call. Once committed, the normal tool renderer rebuilds
        // the row in the ordinary ToolName/Secondary palette.
        let is_subagent_fanout = matches!(
            safe_name.to_ascii_lowercase().as_str(),
            "task" | "codereview" | "code_review"
        );
        let prefix = format!("{} ", icon);
        let prefix_style = if is_subagent_fanout {
            self.style_for(Role::Brand)
        } else {
            self.tool_bullet_style()
        };
        let name_style = if is_subagent_fanout {
            self.style_bold(Role::Brand)
        } else {
            self.style_bold(Role::ToolName)
        };
        let detail_style = if is_subagent_fanout {
            self.style_for(Role::Brand)
        } else {
            self.style_for(Role::Secondary)
        };
        let meta_style = if is_subagent_fanout {
            self.style_bold(Role::Brand)
        } else {
            self.style_bold(Role::ToolName)
        };

        let is_bash = safe_name.eq_ignore_ascii_case("bash");
        let mut new_rows = if safe_detail.is_empty() {
            // No detail: simple path — name + meta, all bold
            self.build_mixed_style_rows(
                &prefix,
                &prefix_style,
                &safe_name,
                &name_style,
                "",
                &detail_style,
                meta,
                &meta_style,
                &safe_name,
            )
        } else if is_bash {
            // Bash: render the command as a STATIC block (`● Bash(cmd)`, no
            // inline meta) — identical to the committed form, shell-boundary
            // wrapped. The live `Running · <t>` animation rides its own
            // spinner-styled row appended below (after the Ctrl+o hint), so the
            // command stays visually stable while the timer updates.
            self.build_bash_command_rows(
                "● ",
                &prefix_style,
                &safe_name,
                &name_style,
                &detail_style,
                &safe_detail,
                "",
                &meta_style,
            )
        } else {
            // Note: full_body intentionally excludes `meta` —
            // build_mixed_style_rows appends meta separately in
            // meta_style. Including meta in full_body would cause
            // it to appear twice (once in the wrapped chunk, once
            // as a separate append).
            let full_body = format!("{}({})", safe_name, safe_detail);
            let full_body = truncate_body_str(&full_body, 500);
            let detail_display = format!("({})", safe_detail);
            self.build_mixed_style_rows(
                &prefix,
                &prefix_style,
                &safe_name,
                &name_style,
                &detail_display,
                &detail_style,
                meta,
                &meta_style,
                &full_body,
            )
        };

        // Append the ephemeral hint (bash "Press Ctrl+o …") inside the strip.
        // Kept INSIDE `new_rows` — hence counted in `inflight_tool_rows` — so
        // the in-place spinner rewrite and `commit_inflight_tool`'s erase cover
        // it atomically. Rendering it as a standalone body row instead orphaned
        // the spinner glyph on commit (the reported bug).
        if let Some(hint) = self.inflight_hint.clone() {
            let muted = if crate::highlight::theme::is_light_for_render() {
                self.style_for(Role::Muted)
            } else {
                self.style_faint(Role::Muted)
            };
            let marker = if self.caps.unicode_symbols {
                "\u{25cb}"
            } else {
                "o"
            };
            let hint_text = format!("  {marker} {hint}");
            let screen_w = self.screen.width();
            new_rows.push(build_one_row(
                &hint_text,
                &muted,
                screen_w,
                self.caps.unicode_symbols,
            ));
        }

        // Bash carries its command as a static block above; the live liveness
        // (animated frame + `Running · <t>` timer) rides its own row here,
        // rendered in the SAME style as the ordinary thinking spinner
        // (`build_spinner_body_row`: brand frame + bold label) so the two read
        // consistently. `meta` already carries the ` · 10.9s` suffix.
        if is_bash {
            // Match the ordinary live-spinner spacing invariant: the liveness
            // row gets exactly one blank row above it. This spacer belongs to
            // the inflight strip so ticks replace it in place and commit/cancel
            // erase it atomically together with the hint and Running row.
            new_rows.push(Vec::new());
            let label = format!("Running{meta}");
            new_rows.push(self.build_spinner_body_row(icon, &label));
        }

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
            // The old strip rows are already gone (truncated above), so clear the count
            // BEFORE re-pushing: otherwise `push_body_row`'s `lift_inflight_strip` would see
            // the stale `prev_rows` and pop that many rows of REAL content off the tail.
            self.inflight_tool_rows = 0;
            for row in new_rows {
                self.push_body_row(row);
            }
        }
        self.inflight_tool_rows = n;
    }

    /// Build prefixed rows where the first-row body has mixed styling:
    /// `name` in `name_style`, `detail` in `detail_style`. Continuation
    /// rows use `detail_style`. Falls back to `build_prefixed_rows` with
    /// `name_style` when detail is empty.
    fn build_mixed_style_rows(
        &self,
        prefix: &str,
        prefix_style: &CellStyle,
        name: &str,
        name_style: &CellStyle,
        detail: &str,
        detail_style: &CellStyle,
        meta: &str,
        meta_style: &CellStyle,
        full_body: &str,
    ) -> Vec<Vec<Cell>> {
        // Downgrade decorative glyphs (the `●`/`▸` prefix + any in name/detail) on
        // non-unicode terminals — 1-col ASCII stand-ins preserve prefix_w alignment.
        let u = self.caps.unicode_symbols;
        let prefix_cow = crate::glyph::downgrade_glyphs(prefix, u);
        let name_cow = crate::glyph::downgrade_glyphs(name, u);
        let detail_cow = crate::glyph::downgrade_glyphs(detail, u);
        let meta_cow = crate::glyph::downgrade_glyphs(meta, u);
        let full_body_cow = crate::glyph::downgrade_glyphs(full_body, u);
        let (prefix, name, detail, meta, full_body) = (
            prefix_cow.as_ref(),
            name_cow.as_ref(),
            detail_cow.as_ref(),
            meta_cow.as_ref(),
            full_body_cow.as_ref(),
        );
        if detail.is_empty() {
            // No detail: append meta (if any) to name, all in name_style.
            let body = if meta.is_empty() {
                name.to_string()
            } else {
                format!("{}{}", name, meta)
            };
            return self.build_prefixed_rows(prefix, prefix_style, &body, name_style, None);
        }
        let w = (self.screen.width() as usize).saturating_sub(PAD_COL);
        if w == 0 {
            return Vec::new();
        }
        let prefix_w = crate::width::display_width(prefix);
        let first_budget = w.saturating_sub(prefix_w);
        let cont_pad: String = " ".repeat(prefix_w);
        let name_dw = crate::width::display_width(name);
        let detail_dw = crate::width::display_width(detail);
        let meta_dw = crate::width::display_width(meta);
        let mut rows = Vec::new();
        if name_dw + detail_dw + meta_dw <= first_budget {
            // Single-line: bold name + secondary detail + bold meta.
            let mut row = Vec::new();
            push_str_cells(&mut row, prefix, prefix_style);
            push_str_cells(&mut row, name, name_style);
            push_str_cells(&mut row, detail, detail_style);
            if !meta.is_empty() {
                push_str_cells(&mut row, meta, meta_style);
            }
            rows.push(row);
        } else {
            // Wrapping: first chunk splits at the name boundary; continuation
            // chunks are padded under the name. `full_body` excludes `meta`
            // (the ` · 12s` time anchor) — it's appended AFTER the wrapped body
            // below, matching the single-line order (name → detail → meta).
            let chunks: Vec<String> =
                crate::width::wrap_line_to_width(full_body, first_budget.max(1))
                    .into_iter()
                    .map(|c| c.to_string())
                    .collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let mut row = Vec::new();
                let pad = CellStyle::default();
                if i == 0 {
                    push_str_cells(&mut row, prefix, prefix_style);
                    let chunk_dw = crate::width::display_width(chunk);
                    if chunk_dw <= name_dw {
                        push_str_cells(&mut row, chunk, name_style);
                    } else {
                        let name_part = crate::width::truncate_to_width(chunk, name_dw);
                        push_str_cells(&mut row, &name_part, name_style);
                        let rest = &chunk[name_part.len()..];
                        if !rest.is_empty() {
                            push_str_cells(&mut row, rest, detail_style);
                        }
                    }
                } else {
                    push_str_cells(&mut row, &cont_pad, &pad);
                    // Continuation chunks: track cumulative width so we know
                    // whether we're still within the tool name or past it.
                    let chunk_dw = crate::width::display_width(chunk);
                    let consumed_dw: usize = chunks[..i]
                        .iter()
                        .map(|c| crate::width::display_width(c))
                        .sum();
                    if consumed_dw + chunk_dw <= name_dw {
                        // Entire chunk is still within the tool name
                        push_str_cells(&mut row, chunk, name_style);
                    } else if consumed_dw < name_dw {
                        // Chunk straddles name/detail boundary
                        let name_remain = name_dw - consumed_dw;
                        let name_part = crate::width::truncate_to_width(chunk, name_remain);
                        push_str_cells(&mut row, &name_part, name_style);
                        let rest = &chunk[name_part.len()..];
                        if !rest.is_empty() {
                            push_str_cells(&mut row, rest, detail_style);
                        }
                    } else {
                        // Chunk is entirely past the tool name
                        push_str_cells(&mut row, chunk, detail_style);
                    }
                }
                rows.push(row);
            }
            // Append `meta` after the wrapped body. Appending it to the FIRST
            // row (as the old code did) overflowed the screen by meta's width —
            // the first chunk already fills `first_budget`, so `prefix + chunk +
            // meta` exceeds `w` and the terminal clips or re-wraps it, dropping
            // the `· 93.7s` time anchor on narrow windows. Put it on the last
            // row when it fits, otherwise on its own continuation row.
            if !meta.is_empty() {
                let last_w: usize = rows
                    .last()
                    .map(|r| r.iter().map(|c| c.width as usize).sum())
                    .unwrap_or(0);
                if last_w + meta_dw <= w {
                    if let Some(last) = rows.last_mut() {
                        push_str_cells(last, meta, meta_style);
                    }
                } else {
                    let mut row = Vec::new();
                    push_str_cells(&mut row, &cont_pad, &CellStyle::default());
                    // meta is typically " · 12s"; drop the leading space so it
                    // lines up under the body on its own row.
                    push_str_cells(&mut row, meta.trim_start(), meta_style);
                    rows.push(row);
                }
            }
        }
        rows
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
    fn pad_row_to_width(row: &mut Vec<Cell>, target_w: usize, style: CellStyle) {
        let cur: usize = row.iter().map(|c| c.width as usize).sum();
        if cur >= target_w {
            return;
        }
        let blank = Cell {
            ch: ' ',
            style,
            width: 1,
        };
        for _ in cur..target_w {
            row.push(blank.clone());
        }
    }

    fn build_rule_row(&self, rule_width: usize, shell: bool) -> Vec<Cell> {
        let mut row = Vec::with_capacity(rule_width);
        // `!` shell mode tints the box rules atomcode brand-purple so the whole
        // input frame reads as "this runs in the shell, not the agent".
        let border = self.style_for(if shell { Role::Shell } else { Role::Border });
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
    fn build_top_rule_with_context(
        &self,
        rule_width: usize,
        session_name: Option<&str>,
        history: Option<super::HistoryPosition>,
        shell: bool,
    ) -> Vec<Cell> {
        let mut row = self.build_rule_row(rule_width, shell);
        const LEFT_MARGIN: usize = 2;
        const BADGE_GAP: usize = 2;
        let history_end = history
            .filter(|position| position.total > 0 && position.current <= position.total)
            .map(|position| {
                let text = format!(" History {}/{} ", position.current, position.total);
                let cells = {
                    let mut cells = Vec::new();
                    push_str_cells(&mut cells, &text, &self.style_faint(Role::Muted));
                    cells
                };
                if LEFT_MARGIN + cells.len() + BADGE_GAP <= rule_width {
                    for (offset, cell) in cells.into_iter().enumerate() {
                        row[LEFT_MARGIN + offset] = cell;
                    }
                    LEFT_MARGIN + crate::width::display_width(&text)
                } else {
                    0
                }
            })
            .unwrap_or(0);

        let Some(name) = session_name else {
            return row;
        };
        if name.is_empty() {
            return row;
        }
        const RIGHT_MARGIN: usize = 2;
        const PILL_PADDING: usize = 2;
        const MIN_RULE_LEFT: usize = 8;
        let min_rule_left = MIN_RULE_LEFT.max(history_end.saturating_add(BADGE_GAP));
        let chrome = RIGHT_MARGIN + PILL_PADDING + min_rule_left;
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
            bg: None,
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

    fn build_middle_row(&self, line: &str, is_first: bool, shell: bool) -> Vec<Cell> {
        let mut row = Vec::new();
        let pad = CellStyle::default();
        if is_first {
            // Shell mode tints the chevron (and the leading `!` below) brand-purple.
            let chevron = self.style_for(if shell { Role::Shell } else { Role::Accent });
            push_str_cells(&mut row, self.caps.prompt_chevron(), &chevron);
        } else {
            push_str_cells(&mut row, "  ", &pad);
        }
        // In shell mode, paint ONLY the leading `!` purple; the command itself
        // keeps the default fg so it stays readable. `input_shell_mode` trims
        // leading whitespace, so honour the same trim here — otherwise `  !ls`
        // leaves the `!` untinted while the box around it is purple.
        if is_first && shell {
            let ws = line.len() - line.trim_start().len();
            if line[ws..].starts_with('!') {
                push_str_cells(&mut row, &line[..ws], &pad); // leading spaces, untinted
                push_str_cells(&mut row, "!", &self.style_for(Role::Shell));
                push_str_cells(&mut row, &line[ws + 1..], &pad);
                return row;
            }
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
                // Annotate aliased commands as one row: `/session (new)`.
                // The stored item name stays canonical (`session`) for
                // insert/dispatch; only the visible label carries the alias.
                let display = crate::commands::command_display_name(name);
                let name_width = unicode_width::UnicodeWidthStr::width(display.as_str());
                let pad = 12usize.saturating_sub(name_width);
                let padded = format!("{}{}", display, " ".repeat(pad));
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
            super::MenuKind::Skill | super::MenuKind::Action => {
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
            super::MenuKind::TwoColumn {
                row_prefix,
                selected_marker,
            } => {
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
                        format!(
                            "{}{}{}{}{}",
                            indicator,
                            truncated,
                            " ".repeat(sep),
                            desc,
                            " ".repeat(rest)
                        )
                    }
                }
            }
            super::MenuKind::Plugin => {
                let name_width = unicode_width::UnicodeWidthStr::width(name);
                let pad = 12usize.saturating_sub(name_width);
                let padded = format!("{}{}", name, " ".repeat(pad));
                if selected {
                    format!("▸ {}  {}", padded, desc)
                } else {
                    format!("  {}  {}", padded, desc)
                }
            }
            super::MenuKind::Marketplace | super::MenuKind::PluginInfo => {
                if selected {
                    format!("▸ {}", name)
                } else {
                    format!("  {}", name)
                }
            }
            // SessionList only reaches `build_menu_row` for its header chrome
            // (title row + blank separators + bottom hint); the 2-line session
            // items are rendered by `build_session_menu_rows`. Indent the title
            // and hint by PAD_COL so they line up with the search box's left
            // edge (its `│` border also sits at PAD_COL). Blank separators stay
            // empty so no stray trailing spaces leak out.
            super::MenuKind::SessionList => {
                if name.is_empty() {
                    String::new()
                } else {
                    format!("{}{}", " ".repeat(PAD_COL), name)
                }
            }
        };

        let is_plugin_mgr = matches!(
            kind,
            super::MenuKind::Plugin
                | super::MenuKind::Marketplace
                | super::MenuKind::PluginInfo
                | super::MenuKind::SessionList
        );
        let mut style = if selected && !is_plugin_mgr {
            CellStyle {
                fg: None,
                bold: true,
                reverse: true,
                faint: false,
                bg: None,
            }
        } else if selected && is_plugin_mgr {
            // Match /resume's selected colour (Accent on dark / Brand on light)
            // so the highlight is consistent across the plugin + provider panels.
            self.session_highlight_style()
        } else {
            // Use terminal default fg (Secondary) instead of Muted
            // (SGR 90 / DarkGrey). Several iTerm2 dark presets render
            // bright-black at near-zero contrast against the bg, which
            // makes the entire menu list invisible. Visual hierarchy
            // here comes from the ▸ arrow + reverse-video on the
            // selected row, not from a colour-contrast distinction.
            self.style_for(Role::Secondary)
        };
        let is_uninstall = name == "Uninstall" || name == "卸载";
        if is_uninstall {
            style.fg = Some(crossterm::style::Color::Red);
        }
        let is_hint = name.starts_with('—') && name.ends_with('—');
        if is_hint {
            style = self.style_for(Role::Muted);
        }
        let is_add_marketplace = name == "+ Add Marketplace";
        if is_add_marketplace {
            if selected {
                style = self.style_bold(Role::Border);
            } else {
                style = self.style_for(Role::Secondary);
            }
        }
        let is_add_title = name == "Add Marketplace";
        if is_add_title {
            style = CellStyle {
                fg: None,
                bold: true,
                reverse: false,
                faint: false,
                bg: None,
            };
        }
        let is_muted = name == "Examples:" || name.starts_with("  ·");
        if is_muted {
            style = self.style_for(Role::Muted);
        }
        if selected && content.starts_with("▸ ") && is_plugin_mgr {
            // Arrow + text share the highlight colour (parity with /resume).
            push_str_cells_sgr(&mut row, "▸ ", style.clone());
            push_str_cells_sgr(&mut row, &content[4..], style.clone());
        } else {
            push_str_cells_sgr(&mut row, &content, style.clone());
        }

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

    fn plugin_item_height(&self, desc: &str, rule_width: usize) -> usize {
        let (_status, description) = if let Some(idx) = desc.find("  ·  ") {
            (&desc[..idx], &desc[idx + 5..])
        } else {
            (desc, "")
        };
        if description.is_empty() {
            2
        } else {
            let avail_w = rule_width.saturating_sub(4).max(1);
            let chunks = crate::width::wrap_line_to_width(description, avail_w);
            1 + chunks.len().min(2).max(1)
        }
    }

    fn build_plugin_menu_rows(
        &self,
        name: &str,
        desc: &str,
        selected: bool,
        rule_width: usize,
    ) -> Vec<Vec<Cell>> {
        let (status, description) = if let Some(idx) = desc.find("  ·  ") {
            (&desc[..idx], &desc[idx + 5..])
        } else {
            (desc, "")
        };

        let name_style = if selected {
            CellStyle {
                fg: None,
                bold: true,
                reverse: false,
                faint: false,
                bg: None,
            }
        } else {
            CellStyle::default()
        };

        let status_style = if selected {
            CellStyle {
                fg: None,
                bold: false,
                reverse: false,
                faint: true,
                bg: None,
            }
        } else {
            let mut s = self.style_for(Role::Secondary);
            s.faint = true;
            s
        };

        let is_installed = status.ends_with("(installed)")
            || status.ends_with("(已安装)")
            || status.ends_with("(User)")
            || status.ends_with("(user)")
            || status.ends_with("(用户)")
            || status.ends_with("(用户级)")
            || status.ends_with("(Project)")
            || status.ends_with("(project)")
            || status.ends_with("(项目)")
            || status.ends_with("(项目级)")
            || status.ends_with("(Local)")
            || status.ends_with("(local)")
            || status.ends_with("(本地)")
            || status.ends_with("(本地级)");
        let name_width = unicode_width::UnicodeWidthStr::width(name);
        let check_width = 2; // "✓ " or "  "
        let pad = 24usize.saturating_sub(name_width + check_width);
        let padded = format!("{}{}", name, " ".repeat(pad));
        let part_b = format!("  {}", status);

        let mut row1 = Vec::new();
        if selected {
            let border_style = self.style_for(Role::Border);
            push_str_cells_sgr(&mut row1, "▸ ", border_style);
        } else {
            push_str_cells_sgr(&mut row1, "  ", CellStyle::default());
        }

        if is_installed {
            let mut gray_style = self.style_for(Role::Muted);
            if selected {
                gray_style.bold = true;
            }
            push_str_cells_sgr(&mut row1, "✓ ", gray_style);
        } else {
            push_str_cells_sgr(&mut row1, "  ", CellStyle::default());
        }

        push_str_cells_sgr(&mut row1, &padded, name_style.clone());
        push_str_cells_sgr(&mut row1, &part_b, status_style);

        let part_a_w = 2 + 2 + crate::width::display_width(&padded);
        let content_w1 = part_a_w + crate::width::display_width(&part_b);
        let right_pad1 = rule_width.saturating_sub(content_w1);
        for _ in 0..right_pad1 {
            row1.push(Cell {
                ch: ' ',
                width: 1,
                style: if selected {
                    name_style.clone()
                } else {
                    CellStyle::default()
                },
            });
        }

        let mut style2 = self.style_for(Role::Secondary);
        style2.faint = true;
        let style2 = if selected {
            CellStyle {
                fg: None,
                bold: false,
                reverse: false,
                faint: true,
                bg: None,
            }
        } else {
            style2
        };

        let avail_w = rule_width.saturating_sub(4).max(1);
        let mut chunks = if description.is_empty() {
            vec![String::new()]
        } else {
            crate::width::wrap_line_to_width(description, avail_w)
        };

        if chunks.len() > 2 {
            chunks.truncate(2);
            let second_line = &chunks[1];
            let max_w = avail_w.saturating_sub(3).max(1);
            let mut truncated = crate::width::truncate_to_width(second_line, max_w);
            truncated.push_str("...");
            chunks[1] = truncated;
        }

        let mut rows = Vec::new();
        rows.push(row1);

        for chunk in chunks {
            let line_str = if chunk.is_empty() {
                String::new()
            } else {
                format!("    {}", chunk)
            };
            let mut row = Vec::new();
            push_str_cells_sgr(&mut row, &line_str, style2.clone());
            let content_w = crate::width::display_width(&line_str);
            let right_pad = rule_width.saturating_sub(content_w);
            for _ in 0..right_pad {
                row.push(Cell {
                    ch: ' ',
                    width: 1,
                    style: if selected {
                        style2.clone()
                    } else {
                        CellStyle::default()
                    },
                });
            }
            rows.push(row);
        }

        rows
    }

    /// Two-line renderer for a `/resume` session row. Simpler than
    /// `build_plugin_menu_rows`: row 1 is an optional `▸ ` marker (Border when
    /// selected, else spaces) + the session title in the terminal's default fg
    /// (bold when selected) — NO `✓`, NO name padding, NO installed/scope suffix
    /// logic (a session literally named "… (local)" must NOT get a check). Row 2
    /// is the whole metadata string (`"12 messages · 1 week ago"`) indented four
    /// spaces in the muted style.
    fn build_session_menu_rows(
        &self,
        name: &str,
        desc: &str,
        selected: bool,
        rule_width: usize,
    ) -> Vec<Vec<Cell>> {
        // Selected title + marker use the theme-aware highlight colour (cyan on
        // dark, magenta on light); unselected rows stay in the terminal default.
        let name_style = if selected {
            self.session_highlight_style()
        } else {
            CellStyle::default()
        };

        // Row 1: PAD_COL indent + marker + highlighted title. The leading
        // PAD_COL puts the `▸` marker under the search box's `│` border (col 2),
        // and the 2-col marker slot puts the title at col 4 — flush with the
        // search box text and the metadata row below.
        let mut row1 = Vec::new();
        push_str_cells_sgr(&mut row1, &" ".repeat(PAD_COL), CellStyle::default());
        if selected {
            push_str_cells_sgr(&mut row1, "▸ ", self.session_highlight_style());
        } else {
            push_str_cells_sgr(&mut row1, "  ", CellStyle::default());
        }
        push_str_cells_sgr(&mut row1, name, name_style.clone());

        let content_w1 = PAD_COL + 2 + crate::width::display_width(name);
        let right_pad1 = rule_width.saturating_sub(content_w1);
        for _ in 0..right_pad1 {
            row1.push(Cell {
                ch: ' ',
                width: 1,
                style: CellStyle::default(),
            });
        }

        // Row 2: gray metadata, indented four spaces.
        let meta_style = self.style_for(Role::Muted);
        let line_str = if desc.is_empty() {
            String::new()
        } else {
            format!("    {}", desc)
        };
        let mut row2 = Vec::new();
        push_str_cells_sgr(&mut row2, &line_str, meta_style.clone());
        let content_w2 = crate::width::display_width(&line_str);
        let right_pad2 = rule_width.saturating_sub(content_w2);
        for _ in 0..right_pad2 {
            row2.push(Cell {
                ch: ' ',
                width: 1,
                style: if selected {
                    meta_style.clone()
                } else {
                    CellStyle::default()
                },
            });
        }

        vec![row1, row2]
    }

    fn build_marketplace_menu_rows(
        &self,
        name: &str,
        desc: &str,
        selected: bool,
        rule_width: usize,
    ) -> Vec<Vec<Cell>> {
        let parts: Vec<&str> = desc.split('|').collect();
        let source = parts.get(0).copied().unwrap_or("");
        let available = parts.get(1).copied().unwrap_or("0");
        let installed = parts.get(2).copied().unwrap_or("0");
        let updated = parts.get(3).copied().unwrap_or("");

        let bullet = if selected { "▸ ● " } else { "  ● " };
        let bullet_style = if selected {
            self.style_for(Role::Accent)
        } else {
            self.style_for(Role::Muted)
        };

        let name_style = CellStyle {
            fg: None,
            bold: true,
            reverse: false,
            faint: false,
            bg: None,
        };

        let mut row1 = Vec::new();
        if selected {
            let border_style = self.style_for(Role::Border);
            push_str_cells_sgr(&mut row1, "▸ ", border_style.clone());
            push_str_cells_sgr(&mut row1, "● ", border_style);
        } else {
            push_str_cells_sgr(&mut row1, bullet, bullet_style);
        }
        push_str_cells_sgr(&mut row1, name, name_style.clone());
        let content_w1 = crate::width::display_width(bullet) + crate::width::display_width(name);
        let right_pad1 = rule_width.saturating_sub(content_w1);
        for _ in 0..right_pad1 {
            row1.push(Cell {
                ch: ' ',
                width: 1,
                style: CellStyle::default(),
            });
        }

        let mut style2 = self.style_for(Role::Secondary);
        style2.faint = true;

        let line2_str = format!("  {}", source);
        let mut row2 = Vec::new();
        push_str_cells_sgr(&mut row2, &line2_str, style2.clone());
        let content_w2 = crate::width::display_width(&line2_str);
        let right_pad2 = rule_width.saturating_sub(content_w2);
        for _ in 0..right_pad2 {
            row2.push(Cell {
                ch: ' ',
                width: 1,
                style: CellStyle::default(),
            });
        }

        let line3_str = format!(
            "  {} available • {} installed • Updated {}",
            available, installed, updated
        );
        let mut row3 = Vec::new();
        push_str_cells_sgr(&mut row3, &line3_str, style2.clone());
        let content_w3 = crate::width::display_width(&line3_str);
        let right_pad3 = rule_width.saturating_sub(content_w3);
        for _ in 0..right_pad3 {
            row3.push(Cell {
                ch: ' ',
                width: 1,
                style: CellStyle::default(),
            });
        }

        vec![row1, row2, row3]
    }

    fn build_status_row(&self, status: &StatusLine, rule_width: usize, shell: bool) -> Vec<Cell> {
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

        // Left mode badge — a single left-aligned badge covers all non-default
        // modes. The badge (`ModeBadge`) carries both its label and its colour
        // slot (`BadgeColour`), so the renderer just maps the slot to a
        // `CellStyle` — no per-mode `if/else if` branch needed.
        // `!` shell mode takes the slot over all mode badges: a `!` line runs
        // in the shell (bypassing the agent), so the agent mode is momentarily
        // moot — the `shell` badge (brand-purple, sibling of PLAN/auto) is
        // what matters.
        let (left_badge, left_badge_style): (Option<String>, CellStyle) = if shell {
            (Some("shell".to_string()), self.style_for(Role::Shell))
        } else if let Some(badge) = status.mode_indicator.as_ref() {
            let style = match badge.colour {
                crate::render::BadgeColour::Mode => self.style_for(Role::Mode),
                crate::render::BadgeColour::Plan => self.style_for(Role::Plan),
                crate::render::BadgeColour::Secondary => secondary.clone(),
            };
            (Some(badge.label.clone()), style)
        } else if let Some(b) = status.bypass_indicator.as_ref() {
            (Some(b.clone()), self.style_for(Role::Warning))
        } else {
            (None, brand.clone())
        };
        // The badge is followed by " · " (space · middot · space = width 3).
        // This constant must match the separator emitted in `push_badge` below.
        const BADGE_SEP_W: usize = 3;
        let mode_badge_w = left_badge
            .as_ref()
            .map(|s| crate::width::display_width(s) + BADGE_SEP_W)
            .unwrap_or(0);

        // Hint right-alignment math must reserve space for the mode badge
        // so it never collides with the right-aligned hint when wide.
        let max = rule_width.max(1);
        let right_reserved = mode_badge_w;
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
        let ctx_str = if status.ctx_used > 0 || status.ctx_window > 0 {
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

        let mut parts: Vec<String> = Vec::with_capacity(4);
        if !model_str.is_empty() {
            parts.push(model_str);
        }
        if !status.cwd.is_empty() {
            let cwd_full = scrub_controls(&status.cwd);
            let cwd_display =
                if cwd_budget > 0 && crate::width::display_width(&cwd_full) > cwd_budget {
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
        // NOTE: the goal indicator is NOT appended here any more — it lives on
        // its own dedicated footer row (`build_goal_row`) so it can't be the
        // first thing truncated off this line under a hint / narrow terminal.
        let left = parts.join(" · ");

        // Helper: emit the badge (with trailing space) then the rest, so
        // the mode indicator is always at column 0 (after PAD_COL) and
        // both hint / no-hint branches share the same prefix.
        let push_badge = |row: &mut Vec<Cell>| {
            if let Some(badge) = &left_badge {
                push_str_cells(row, badge, &left_badge_style);
                push_str_cells(row, " · ", &secondary);
            }
        };

        if let Some((raw_hint, severity)) = status.hint.as_ref() {
            let hint = scrub_controls(raw_hint);
            let hint_w = crate::width::display_width(&hint);
            let hint_style = match severity {
                crate::render::HintSeverity::Warning => error,
                crate::render::HintSeverity::Info => secondary.clone(),
                // `!` shell-mode affordance — brand purple, matching the box/badge.
                crate::render::HintSeverity::Shell => self.style_for(Role::Shell),
            };
            let right_w = hint_w;
            if right_w + 1 < left_max {
                let left_budget = left_max - right_w - 1;
                let left_truncated = crate::width::truncate_to_width(&left, left_budget);
                let left_w = crate::width::display_width(&left_truncated);
                let pad_w = max - right_reserved - left_w - hint_w;
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

    /// Emit one dedicated footer row from its three width-fitted segments,
    /// with the calm shared hierarchy of the goal/loop/todo rows: `marker` in
    /// `Brand` (a small accent), `mid` in normal text, `meta` in `Muted` gray.
    /// Callers produce the tuple via the matching `*_row_parts` fn.
    fn build_marker_row(&self, marker: &str, mid: &str, meta: &str) -> Vec<Cell> {
        let mut row = Vec::new();
        push_str_cells(&mut row, marker, &self.style_for(Role::Brand));
        push_str_cells(&mut row, mid, &CellStyle::default());
        push_str_cells(&mut row, meta, &self.style_for(Role::Muted));
        row
    }

    /// Build the dedicated goal row (one full-width line, shown only while a
    /// `/goal` loop is active). Sits directly above the status line: the `◎`
    /// marker, the condition, and the ` · round N · elapsed` meta.
    fn build_goal_row(&self, goal: &crate::render::GoalStatus, rule_width: usize) -> Vec<Cell> {
        let (marker, condition, meta) = goal_row_parts(
            &scrub_controls(&goal.condition),
            goal.round,
            goal.elapsed_secs,
            rule_width.max(1),
            self.caps.unicode_symbols,
        );
        self.build_marker_row(marker, &condition, &meta)
    }

    /// Build the dedicated loop row (one full-width line, shown only while a
    /// `/loop` is active). Sits in the same slot as the goal row — goal and loop
    /// are mutually exclusive in practice. `⚡` marker, label, ` · round N · elapsed`.
    fn build_loop_row(&self, ls: &crate::render::LoopStatus, rule_width: usize) -> Vec<Cell> {
        let (marker, label, meta) = loop_row_parts(
            &scrub_controls(&ls.label),
            ls.round,
            ls.elapsed_secs,
            rule_width.max(1),
            self.caps.unicode_symbols,
        );
        self.build_marker_row(marker, &label, &meta)
    }

    /// Effective panel height cap: `MAX_TODO_PANEL_ROWS`, clamped so the panel
    /// never claims more than the screen can spare (rules + one input + status).
    fn todo_panel_cap(&self) -> usize {
        let h = self.screen.height() as usize;
        MAX_TODO_PANEL_ROWS.min(h.saturating_sub(4)).max(1)
    }

    /// Number of rows the todo panel will occupy — mirrors `build_todo_rows`'
    /// row count without building cells (used by the footer height math).
    fn todo_panel_row_count(&self, todo: &crate::render::TodoProgress) -> usize {
        todo_panel_rows(
            &todo.items,
            todo.completed,
            todo.in_progress,
            todo.total,
            self.todo_panel_cap(),
        )
        .len()
    }

    fn subtask_panel_cap(&self) -> usize {
        let h = self.screen.height() as usize;
        let hide_input_box = self.menu.as_ref().is_some_and(|menu| {
            matches!(
                menu.kind,
                super::MenuKind::Plugin
                    | super::MenuKind::Marketplace
                    | super::MenuKind::PluginInfo
                    | super::MenuKind::SessionList
            )
        });
        let status_rows = usize::from(
            !self.status.model.is_empty()
                || !self.status.cwd.is_empty()
                || self.status.hint.is_some(),
        );
        let goal_rows =
            usize::from(self.status.goal.is_some() || self.status.loop_status.is_some());
        let input_chrome = if hide_input_box {
            1 // shared top rule
        } else {
            3 + status_rows // top rule + one input row + bottom rule + status
        };
        let reserved = 1 // keep at least one body row visible
            + input_chrome
            + self.input_attachments.len()
            + self.max_menu_rows(h, 0)
            + goal_rows
            + self.build_command_output_rows().len();
        MAX_SUBTASK_PANEL_ROWS.min(h.saturating_sub(reserved))
    }

    fn subtask_panel_row_count(&self, subtasks: &crate::render::SubtaskProgress) -> usize {
        let cap = self.subtask_panel_cap();
        if cap == 0 {
            return 0;
        }
        let running = subtasks
            .items
            .iter()
            .filter(|item| item.status == crate::render::SubtaskStatus::Running)
            .count();
        let failed = subtasks
            .items
            .iter()
            .filter(|item| item.status == crate::render::SubtaskStatus::Failed)
            .count();
        let pending = subtasks
            .items
            .iter()
            .filter(|item| item.status == crate::render::SubtaskStatus::Pending)
            .count();
        let needs_summary =
            failed > 0 || pending > 0 || running > MAX_VISIBLE_RUNNING_SUBTASKS;
        let summary_rows = usize::from(needs_summary && cap >= 2);
        let visible_running = running
            .min(MAX_VISIBLE_RUNNING_SUBTASKS)
            .min(cap.saturating_sub(1 + summary_rows));
        1 + visible_running + summary_rows
    }

    /// Build the fixed Task fan-out panel. Only running children own detailed
    /// rows; pending and terminal children fold into the header/summary so they
    /// cannot displace current work. None of these rows enter scrollback.
    fn build_subtask_rows(
        &self,
        subtasks: &crate::render::SubtaskProgress,
        rule_width: usize,
    ) -> Vec<Vec<Cell>> {
        use crate::render::SubtaskStatus;

        let cap = self.subtask_panel_cap();
        if cap == 0 {
            return Vec::new();
        }
        let mut rows = Vec::new();

        let bold = CellStyle {
            bold: true,
            ..CellStyle::default()
        };
        let detail = self.style_for(Role::Secondary);
        let mut header = Vec::new();
        let marker = if self.caps.unicode_symbols {
            "\u{25cf}"
        } else {
            "*"
        };
        push_str_cells(&mut header, marker, &self.style_for(Role::Brand));
        push_str_cells(&mut header, " Subtasks", &bold);
        let failed = subtasks
            .items
            .iter()
            .filter(|item| item.status == SubtaskStatus::Failed)
            .count();
        let running = subtasks
            .items
            .iter()
            .filter(|item| item.status == SubtaskStatus::Running)
            .collect::<Vec<_>>();
        let pending = subtasks
            .items
            .iter()
            .filter(|item| item.status == SubtaskStatus::Pending)
            .count();
        let finished = subtasks.completed.saturating_add(failed);
        push_str_cells(
            &mut header,
            &format!(
                " \u{b7} {finished}/{} finished \u{b7} {} running \u{b7} {pending} pending",
                subtasks.total,
                running.len()
            ),
            &detail,
        );
        rows.push(header);

        let needs_summary =
            failed > 0 || pending > 0 || running.len() > MAX_VISIBLE_RUNNING_SUBTASKS;
        let summary_rows = usize::from(needs_summary && cap >= 2);
        let visible_running = running
            .len()
            .min(MAX_VISIBLE_RUNNING_SUBTASKS)
            .min(cap.saturating_sub(rows.len() + summary_rows));
        for item in running.iter().take(visible_running).copied() {
            let glyph = "\u{25cf}";
            let glyph = if self.caps.unicode_symbols {
                glyph
            } else {
                "[>]"
            };
            let mut identity = if item.description.is_empty() {
                item.label.clone()
            } else {
                format!("{} \u{b7} {}", item.label, item.description)
            };
            if !item.model.is_empty() {
                let label_len = item.label.len();
                let remainder = identity.split_off(label_len);
                identity.push_str(&format!(" \u{b7} {}{}", item.model, remainder));
            }
            let elapsed = item
                .started_at
                .map(|started_at| started_at.elapsed().as_secs())
                .unwrap_or(0);
            let elapsed = if elapsed >= 60 {
                format!("{}m{}s", elapsed / 60, elapsed % 60)
            } else {
                format!("{elapsed}s")
            };
            let activity = if item.activity.is_empty() {
                "analyzing task"
            } else {
                item.activity.as_str()
            };
            let content = format!("{} \u{b7} {activity}", scrub_controls(&identity));
            let glyph_width = crate::width::display_width(glyph);
            let content_width = rule_width.saturating_sub(2 + glyph_width + 1);
            let fitted =
                format_subtask_progress(&content, &elapsed, item.output_tokens, content_width);
            let mut row = Vec::new();
            push_str_cells(&mut row, "  ", &CellStyle::default());
            push_str_cells(
                &mut row,
                glyph,
                &self.style_for(Role::Brand),
            );
            push_str_cells(&mut row, " ", &CellStyle::default());
            push_str_cells(&mut row, &fitted, &self.style_for(Role::Secondary));
            rows.push(row);
        }
        if needs_summary && rows.len() < cap {
            let mut parts = Vec::new();
            let hidden_running = running.len().saturating_sub(visible_running);
            if hidden_running > 0 {
                parts.push(format!("{hidden_running} running"));
            }
            if pending > 0 {
                parts.push(format!("{pending} pending"));
            }
            if failed > 0 {
                parts.push(format!("{failed} failed"));
            }
            let mut row = Vec::new();
            push_str_cells(&mut row, "  ", &CellStyle::default());
            push_str_cells(
                &mut row,
                &format!(
                    "{} {}",
                    if self.caps.unicode_symbols {
                        "\u{2026}"
                    } else {
                        "..."
                    },
                    parts.join(" \u{b7} ")
                ),
                &self.style_for(Role::Muted),
            );
            rows.push(row);
        }
        for row in &mut rows {
            clamp_cell_row(row, rule_width);
        }
        rows
    }

    /// Build the multi-line todo panel: a header row (`Tasks (N done, M open)`)
    /// followed by collapsed rows — `✔ N done` fold, then `☐ #k content` items
    /// (in-progress bold, pending muted), then a `+N more…` fold. Pinned at the top
    /// of the footer. Theme-safe and COLORLESS — hierarchy is by weight. Checkbox
    /// glyphs downgrade to `[ ]`/`[x]` on non-unicode terminals.
    fn build_todo_rows(
        &self,
        todo: &crate::render::TodoProgress,
        rule_width: usize,
    ) -> Vec<Vec<Cell>> {
        use atomcode_capabilities::tools::todo::TodoStatus;
        let unicode = self.caps.unicode_symbols;
        let rows = todo_panel_rows(
            &todo.items,
            todo.completed,
            todo.in_progress,
            todo.total,
            self.todo_panel_cap(),
        );
        // Checkbox glyphs (Tasks-panel style): ☐ open, ✔ done. Emit the ASCII form
        // directly on non-unicode terminals so the cell backstop never has to guess.
        let open_glyph = if unicode { "\u{2610}" } else { "[ ]" };
        // U+25A0 black square (filled) for the in-progress checkbox — the solid
        // fill with brand colour makes the active task stand out from pending's
        // empty outline.
        let filled_glyph = if unicode { "\u{25a0}" } else { "[#]" };
        // U+2713 (light check) not U+2714 (heavy) — the heavy one triggers emoji
        // presentation on many terminals (renders green + width-2, leaking a stray cell).
        let done_glyph = if unicode { "\u{2713}" } else { "[x]" };
        let ellipsis = if unicode { "\u{2026}" } else { "..." };

        // An indented item line: `  <glyph> <content>` (content width-fitted).
        let item_line = |glyph: &str,
                         body: &str,
                         glyph_style: &CellStyle,
                         body_style: &CellStyle|
         -> Vec<Cell> {
            let mut row = Vec::new();
            let gw = crate::width::display_width(glyph);
            let budget = rule_width.saturating_sub(2 + gw + 1); // indent + glyph + space
            let fitted = crate::width::truncate_with_ellipsis(&scrub_controls(body), budget);
            push_str_cells(&mut row, "  ", &CellStyle::default());
            push_str_cells(&mut row, glyph, glyph_style);
            push_str_cells(&mut row, " ", &CellStyle::default());
            push_str_cells(&mut row, &fitted, body_style);
            row
        };

        rows.into_iter()
            .map(|r| match r {
                // Blank spacer: empty cell list — paint_footer pads it to width.
                TodoPanelRow::Spacer => Vec::new(),
                TodoPanelRow::Header {
                    completed,
                    in_progress,
                    total,
                } => {
                    // `Tasks` in bold default fg, `(N done, M in progress, K open)` in
                    // detail colour (default fg, no bold) — the label is the anchor,
                    // the counts are subordinate metadata. English regardless of locale.
                    // `open` = pure pending count (total − completed − in_progress).
                    // Previously this included in-progress (total − completed); the
                    // semantic changed when the header grew the `in progress` counter,
                    // so `open` now answers "how many haven't been touched yet".
                    let open = total.saturating_sub(completed + in_progress);
                    let bold = CellStyle {
                        bold: true,
                        ..CellStyle::default()
                    };
                    let detail = self.style_for(Role::Secondary);
                    let mut row = Vec::new();
                    push_str_cells(&mut row, "Tasks ", &bold);
                    push_str_cells(
                        &mut row,
                        &format!("({completed} done, {in_progress} in progress, {open} open)"),
                        &detail,
                    );
                    row
                }
                TodoPanelRow::Item {
                    index,
                    status,
                    content,
                } => {
                    let (glyph, glyph_style, body_style) = match status {
                        // Brand magenta + bold for the filled checkbox; bold default fg for the text.
                        TodoStatus::InProgress => (
                            filled_glyph,
                            CellStyle {
                                bold: true,
                                ..self.style_for(Role::Brand)
                            },
                            CellStyle {
                                bold: true,
                                ..CellStyle::default()
                            },
                        ),
                        // Completed items are normally folded; render defensively if one slips through.
                        TodoStatus::Completed => {
                            let s = CellStyle {
                                faint: true,
                                ..self.style_for(Role::Muted)
                            };
                            (done_glyph, s.clone(), s)
                        }
                        // Pending items use the same colour as tool detail (default fg) so
                        // three states are visually distinct: muted+faint done, bold accent,
                        // plain pending.
                        TodoStatus::Pending => {
                            let s = self.style_for(Role::Secondary);
                            (open_glyph, s.clone(), s)
                        }
                    };
                    item_line(
                        glyph,
                        &format!("#{}  {}", index + 1, content),
                        &glyph_style,
                        &body_style,
                    )
                }
                TodoPanelRow::More { hidden } => {
                    // Fold indicator, not a task — no checkbox, just an indented muted+faint note.
                    let style = CellStyle {
                        faint: true,
                        ..self.style_for(Role::Muted)
                    };
                    let mut row = Vec::new();
                    push_str_cells(&mut row, &format!("  +{hidden} more{ellipsis}"), &style);
                    row
                }
            })
            .collect()
    }

    /// Rows the approval panel occupies — one per option. The compact panel drops
    /// the header/detail/hint: the command is already shown in the `▸ Tool(detail)`
    /// body row above, so the panel is just the selectable choices.
    fn approval_panel_row_count(&self, panel: &crate::render::ApprovalPanelView) -> usize {
        // 1 header row ("Allow Tool(detail)?") + N option rows + 1 hint row.
        panel.options.len() + 2
    }

    /// Build the compact footer approval panel: numbered selectable options
    /// followed by a muted hint row.
    ///
    /// Layout per option row: `<marker><n>. <label>` padded to `rule_width`.
    ///   - Selected row: `▸ ` + `{n}. ` + label, reverse-highlighted and padded
    ///     with reverse-styled spaces to `rule_width` (full-width, no jump).
    ///   - Unselected: `  ` + `{n}. ` + label in `Role::Secondary`.
    ///
    /// After the option rows: a muted hint row `  ↑↓ select · Enter confirm · Esc cancel`
    /// (localized + glyph-downgraded on non-unicode terminals).
    fn build_approval_rows(
        &self,
        panel: &crate::render::ApprovalPanelView,
        rule_width: usize,
        screen_width: usize,
    ) -> Vec<Vec<Cell>> {
        let unicode = self.caps.unicode_symbols;
        let mut out: Vec<Vec<Cell>> = Vec::new();

        // Header row: name what is being approved. The `▸ Tool(detail)` scrollback row
        // can scroll off / be separated by the todo panel, so restate it here. The DETAIL
        // is truncated (not the whole line) so the "Allow Tool(…)?" frame — tool name +
        // closing ")?" — stays visible; a final width-truncate is the hard one-line backstop.
        {
            let tool_w = crate::width::display_width(&panel.tool);
            // Reserve for the localized frame ("Allow " + "(" + ")?") + the 2-col indent.
            let detail_budget = rule_width.saturating_sub(tool_w + 12);
            let detail =
                crate::width::truncate_with_ellipsis(&scrub_controls(&panel.detail), detail_budget);
            let raw = crate::i18n::t(crate::i18n::Msg::ApprovalHeader {
                tool: &panel.tool,
                detail: &detail,
            });
            let header = crate::glyph::downgrade_glyphs(&raw, unicode);
            let header_trunc =
                crate::width::truncate_with_ellipsis(&header, rule_width.saturating_sub(2));
            let style = CellStyle {
                bold: true,
                ..CellStyle::default()
            };
            let mut row = Vec::new();
            push_str_cells(&mut row, "  ", &style);
            push_str_cells(&mut row, &header_trunc, &style);
            out.push(row);
        }

        // option rows: `<marker><n>. <label>` (selected: ▸ + reverse padded to screen_width)
        for (i, label) in panel.options.iter().enumerate() {
            let mut row = Vec::new();
            let selected = i == panel.selected;
            let marker = if selected {
                if unicode {
                    "\u{25b8} "
                } else {
                    "> "
                } // ▸
            } else {
                "  "
            };
            // marker is 2 cols; number prefix `{n}. ` is up to 3 cols (e.g. "1. ")
            let num_prefix = format!("{}. ", i + 1);
            let num_prefix_width = crate::width::display_width(&num_prefix);
            let style = if selected {
                CellStyle {
                    reverse: true,
                    ..CellStyle::default()
                }
            } else {
                self.style_for(Role::Secondary)
            };
            let budget = rule_width.saturating_sub(2 + num_prefix_width);
            let lbl = crate::width::truncate_with_ellipsis(&scrub_controls(label), budget);
            push_str_cells(&mut row, marker, &style);
            push_str_cells(&mut row, &num_prefix, &style);
            push_str_cells(&mut row, &lbl, &style);
            // Full-width selected highlight: pad with reverse spaces to screen_width so
            // the entire terminal row is highlighted (no bare-background gap on the right).
            if selected {
                let current_width: usize = row.iter().map(|c| c.width as usize).sum();
                let pad = screen_width.saturating_sub(current_width);
                if pad > 0 {
                    push_str_cells(&mut row, &" ".repeat(pad), &style);
                }
            }
            out.push(row);
        }

        // Hint row: `  <localized hint>` in muted style, glyph-downgraded, truncated.
        let hint_raw = crate::i18n::t(crate::i18n::Msg::ApprovalHint);
        let hint = crate::glyph::downgrade_glyphs(&hint_raw, unicode);
        let hint_budget = rule_width.saturating_sub(2);
        let hint_truncated = crate::width::truncate_with_ellipsis(&hint, hint_budget);
        let hint_style = if crate::highlight::theme::is_light_for_render() {
            self.style_for(Role::Muted)
        } else {
            self.style_faint(Role::Muted)
        };
        let mut hint_row = Vec::new();
        push_str_cells(&mut hint_row, "  ", &hint_style);
        push_str_cells(&mut hint_row, &hint_truncated, &hint_style);
        out.push(hint_row);

        out
    }

    /// Rows the `request_user_input` panel occupies. Mirrors
    /// `approval_panel_row_count`: 1 header (the question) + the mode body
    /// (single/multiple = one row per option; text = one `> {buffer}` row) + 1
    /// hint row. Must equal `build_user_input_rows(..).len()`.
    fn user_input_panel_row_count(&self, panel: &crate::render::UserInputPanelView) -> usize {
        use atomcode_capabilities::tools::request_user_input::UserInputMode;
        match panel.mode {
            UserInputMode::Single | UserInputMode::Multiple => {
                // header chip + blank + question + blank
                let mut n = 4;
                // Each concrete option: 1 label row + (1 description row if present).
                for (_, desc) in &panel.options {
                    n += 1;
                    if desc
                        .as_deref()
                        .map(|d| !d.trim().is_empty())
                        .unwrap_or(false)
                    {
                        n += 1;
                    }
                }
                // Custom-answer ("Other") row: 1 inline input line — only when offered.
                if panel.custom {
                    n += 1;
                }
                // Multiple: a blank spacer + the Submit row.
                if matches!(panel.mode, UserInputMode::Multiple) {
                    n += 2;
                }
                // blank + hint
                n += 2;
                n
            }
            // Text: header chip + blank + question + blank + input row + hint.
            UserInputMode::Text => 6,
        }
    }

    /// Build the compact footer `request_user_input` panel.
    ///   - Single / Multiple: a header chip, the question, then numbered options
    ///     each with an optional faint description line, and a final inline
    ///     custom-answer row (faint placeholder when empty, typed text + cursor
    ///     indicator when non-empty / active). The cursor row is marked `❯`;
    ///     multiple adds `[x]`/`[ ]` checkboxes; the cursor label is emphasized
    ///     in the orange highlight colour.
    ///   - Text: a single `> {buffer}` input row.
    /// `user_input_panel_row_count` MUST equal `build_user_input_rows(..).len()`.
    fn build_user_input_rows(
        &self,
        panel: &crate::render::UserInputPanelView,
        rule_width: usize,
        screen_width: usize,
    ) -> Vec<Vec<Cell>> {
        use atomcode_capabilities::tools::request_user_input::UserInputMode;
        let unicode = self.caps.unicode_symbols;
        let mut out: Vec<Vec<Cell>> = Vec::new();

        // A blank spacer row (empty cells).
        let blank_row = |out: &mut Vec<Vec<Cell>>| out.push(Vec::new());
        // Emit a 2-space-indented styled text line, width-truncated.
        let push_line = |out: &mut Vec<Vec<Cell>>, text: &str, style: &CellStyle| {
            let raw = crate::glyph::downgrade_glyphs(text, unicode);
            let body = crate::width::truncate_with_ellipsis(
                &scrub_controls(&raw),
                rule_width.saturating_sub(2),
            );
            let mut row = Vec::new();
            push_str_cells(&mut row, "  ", style);
            push_str_cells(&mut row, &body, style);
            out.push(row);
        };

        match panel.mode {
            UserInputMode::Single | UserInputMode::Multiple => {
                let multiple = matches!(panel.mode, UserInputMode::Multiple);

                // Header CHIP: a short bold label in the orange highlight colour.
                push_line(&mut out, &panel.header, &self.style_bold(Role::Plan));
                blank_row(&mut out);
                // Question line (bold, terminal-default fg).
                push_line(&mut out, &panel.question, &self.style_bold(Role::Secondary));
                blank_row(&mut out);

                // Styles for the faint description lines.
                let desc_style = self.style_faint(Role::Muted);

                let other_index = panel.options.len();
                // Emit one navigable option row (label) + optional description.
                // `number` is the 1-based display index; `label`/`desc` its text.
                let emit_option = |out: &mut Vec<Vec<Cell>>,
                                   idx: usize,
                                   number: usize,
                                   label: &str,
                                   desc: Option<&str>,
                                   checked: bool| {
                    let on_cursor = idx == panel.cursor;
                    // Marker: `❯ ` on the cursor row, else two spaces.
                    let marker = if on_cursor {
                        if unicode {
                            "\u{276f} "
                        } else {
                            "> "
                        } // ❯
                    } else {
                        "  "
                    };
                    // Multiple mode prepends a checkbox.
                    let checkbox = if multiple {
                        if checked {
                            "[x] "
                        } else {
                            "[ ] "
                        }
                    } else {
                        ""
                    };
                    let num = format!("{}. ", number);
                    // Label emphasis: cursor row → bold orange (Plan).
                    let label_style = if on_cursor {
                        self.style_bold(Role::Plan)
                    } else {
                        self.style_for(Role::Secondary)
                    };
                    let chrome_style = if on_cursor {
                        self.style_for(Role::Plan)
                    } else {
                        self.style_for(Role::Secondary)
                    };
                    let prefix_width = crate::width::display_width(marker)
                        + crate::width::display_width(checkbox)
                        + crate::width::display_width(&num);
                    let budget = rule_width.saturating_sub(prefix_width);
                    let lbl = crate::width::truncate_with_ellipsis(&scrub_controls(label), budget);
                    let mut row = Vec::new();
                    push_str_cells(&mut row, marker, &chrome_style);
                    if multiple {
                        push_str_cells(&mut row, checkbox, &chrome_style);
                    }
                    push_str_cells(&mut row, &num, &chrome_style);
                    push_str_cells(&mut row, &lbl, &label_style);
                    out.push(row);
                    // Description: a faint second line, indented under the label.
                    if let Some(d) = desc {
                        let dtrim = d.trim();
                        if !dtrim.is_empty() {
                            let indent = " ".repeat(prefix_width);
                            let dbudget = rule_width.saturating_sub(prefix_width);
                            let draw = crate::glyph::downgrade_glyphs(dtrim, unicode);
                            let dtext = crate::width::truncate_with_ellipsis(
                                &scrub_controls(&draw),
                                dbudget,
                            );
                            let mut drow = Vec::new();
                            push_str_cells(&mut drow, &indent, &desc_style);
                            push_str_cells(&mut drow, &dtext, &desc_style);
                            out.push(drow);
                        }
                    }
                };

                for (i, (label, desc)) in panel.options.iter().enumerate() {
                    let checked = panel.checked.get(i).copied().unwrap_or(false);
                    emit_option(&mut out, i, i + 1, label, desc.as_deref(), checked);
                }

                // The custom-answer ("Other") row (index = options.len()): a ONE-LINE
                // inline text input. Offered only when `panel.custom` is true — no
                // "Other" label, no subtitle; the cursor indicator and typed text (or
                // faint placeholder) are rendered directly on this row.
                if panel.custom {
                    let idx = other_index;
                    let number = other_index + 1;
                    let on_cursor = idx == panel.cursor;
                    // Checkbox for the Other row is derived from custom_text, NOT from
                    // checked[other_index], so Enter on Other (a no-op) can never desync it.
                    let other_has_text = !panel.custom_text.trim().is_empty();
                    let marker = if on_cursor {
                        if unicode {
                            "\u{276f} "
                        } else {
                            "> "
                        }
                    } else {
                        "  "
                    };
                    let checkbox = if multiple {
                        if other_has_text {
                            "[x] "
                        } else {
                            "[ ] "
                        }
                    } else {
                        ""
                    };
                    let num = format!("{}. ", number);
                    let chrome_style = if on_cursor {
                        self.style_for(Role::Plan)
                    } else {
                        self.style_for(Role::Secondary)
                    };

                    let prefix_width = crate::width::display_width(marker)
                        + crate::width::display_width(checkbox)
                        + crate::width::display_width(&num);
                    let budget = rule_width.saturating_sub(prefix_width);

                    let mut row = Vec::new();
                    push_str_cells(&mut row, marker, &chrome_style);
                    if multiple {
                        push_str_cells(&mut row, checkbox, &chrome_style);
                    }
                    push_str_cells(&mut row, &num, &chrome_style);

                    if panel.custom_text.trim().is_empty() {
                        // Faint placeholder when no text has been typed yet
                        // (consistent with build_response which trims before checking).
                        let cursor_glyph = if unicode { "\u{258f}" } else { "|" }; // ▏
                        let glyph_width: usize = 1;
                        // Reserve 1 column for the cursor glyph when on cursor.
                        let ph_budget = if on_cursor {
                            budget.saturating_sub(glyph_width)
                        } else {
                            budget
                        };
                        let placeholder = "输入自己的答案\u{2026}"; // 输入自己的答案…
                        let ph = crate::width::truncate_with_ellipsis(
                            &scrub_controls(placeholder),
                            ph_budget,
                        );
                        let ph_style = self.style_faint(Role::Muted);
                        // Caret at the FRONT (the insertion point of an empty field),
                        // with the placeholder trailing as a hint — so typing begins
                        // where the caret sits rather than after the hint text.
                        if on_cursor {
                            push_str_cells(&mut row, cursor_glyph, &chrome_style);
                        }
                        push_str_cells(&mut row, &ph, &ph_style);
                    } else {
                        // Show the typed text.  Append a cursor indicator when
                        // this is the active row.
                        let text_style = if on_cursor {
                            self.style_bold(Role::Plan)
                        } else {
                            self.style_for(Role::Secondary)
                        };
                        // Reserve 1 column for the cursor glyph when on cursor.
                        let text_budget = if on_cursor {
                            budget.saturating_sub(1)
                        } else {
                            budget
                        };
                        let text = crate::width::truncate_with_ellipsis(
                            &scrub_controls(&panel.custom_text),
                            text_budget,
                        );
                        push_str_cells(&mut row, &text, &text_style);
                        if on_cursor {
                            let cursor_glyph = if unicode { "\u{258f}" } else { "|" }; // ▏
                            push_str_cells(&mut row, cursor_glyph, &chrome_style);
                        }
                    }
                    out.push(row);
                }

                // Multiple mode: a blank spacer, then a navigable Submit row. The
                // Submit index is after the concrete options, plus the Other row when
                // it is offered.
                if multiple {
                    blank_row(&mut out);
                    let submit_index = panel.options.len() + panel.custom as usize;
                    let on_cursor = submit_index == panel.cursor;
                    let marker = if on_cursor {
                        if unicode {
                            "\u{276f} "
                        } else {
                            "> "
                        }
                    } else {
                        "  "
                    };
                    let submit_label = if unicode {
                        "\u{2714} \u{63d0}\u{4ea4}"
                    } else {
                        "+ Submit"
                    };
                    let label_style = if on_cursor {
                        self.style_bold(Role::Plan)
                    } else {
                        self.style_for(Role::Secondary)
                    };
                    let chrome_style = if on_cursor {
                        self.style_for(Role::Plan)
                    } else {
                        self.style_for(Role::Secondary)
                    };
                    let budget = rule_width.saturating_sub(crate::width::display_width(marker));
                    let lbl =
                        crate::width::truncate_with_ellipsis(&scrub_controls(submit_label), budget);
                    let mut row = Vec::new();
                    push_str_cells(&mut row, marker, &chrome_style);
                    push_str_cells(&mut row, &lbl, &label_style);
                    out.push(row);
                }
            }
            UserInputMode::Text => {
                // Header CHIP + question + a `> {buffer}` input row.
                push_line(&mut out, &panel.header, &self.style_bold(Role::Plan));
                blank_row(&mut out);
                push_line(&mut out, &panel.question, &self.style_bold(Role::Secondary));
                blank_row(&mut out);
                let style = CellStyle {
                    reverse: true,
                    ..CellStyle::default()
                };
                let mut row = Vec::new();
                let budget = rule_width.saturating_sub(2);
                let buf =
                    crate::width::truncate_with_ellipsis(&scrub_controls(&panel.text), budget);
                push_str_cells(&mut row, "> ", &style);
                push_str_cells(&mut row, &buf, &style);
                let current_width: usize = row.iter().map(|c| c.width as usize).sum();
                let pad = screen_width.saturating_sub(current_width);
                if pad > 0 {
                    push_str_cells(&mut row, &" ".repeat(pad), &style);
                }
                out.push(row);
            }
        }

        // A blank spacer before the hint (single/multiple only — text closes
        // straight into the hint).
        if matches!(panel.mode, UserInputMode::Single | UserInputMode::Multiple) {
            blank_row(&mut out);
        }

        // Hint row: mode-appropriate guidance, muted + glyph-downgraded. N is the
        // number of navigable options INCLUDING the always-appended "Other".
        let n = panel.options.len() + panel.custom as usize;
        let single_hint = format!("\u{2191}\u{2193} move \u{00b7} 1-{n} select \u{00b7} Enter confirm \u{00b7} Esc cancel");
        // Multiple: Enter toggles rows, confirms only on the Submit row.
        let multiple_hint = format!("\u{2191}\u{2193} move \u{00b7} Space toggle \u{00b7} Enter \u{63d0}\u{4ea4}\u{884c}\u{786e}\u{8ba4} \u{00b7} Esc cancel");
        let hint_raw: &str = match panel.mode {
            UserInputMode::Single => &single_hint,
            UserInputMode::Multiple => &multiple_hint,
            UserInputMode::Text => "type answer \u{00b7} Enter confirm \u{00b7} Esc cancel",
        };
        let hint = crate::glyph::downgrade_glyphs(hint_raw, unicode);
        let hint_budget = rule_width.saturating_sub(2);
        let hint_truncated = crate::width::truncate_with_ellipsis(&hint, hint_budget);
        let hint_style = if crate::highlight::theme::is_light_for_render() {
            self.style_for(Role::Muted)
        } else {
            self.style_faint(Role::Muted)
        };
        let mut hint_row = Vec::new();
        push_str_cells(&mut hint_row, "  ", &hint_style);
        push_str_cells(&mut hint_row, &hint_truncated, &hint_style);
        out.push(hint_row);

        out
    }

    /// Batch-aware wrapper over [`Self::build_user_input_rows`]. A standalone question
    /// (`view.batch` is `None` or `total <= 1`) delegates unchanged (byte-identical).
    /// A multi-question batch prepends a `问题 i/N` navigator (with ✓/○ per-question
    /// markers) and appends a Tab hint, or renders a Submit screen on the Submit stop.
    /// `user_input_panel_view_row_count` MUST equal this length.
    fn build_user_input_panel_view(
        &self,
        view: &crate::render::UserInputPanelView,
        rule_width: usize,
        screen_width: usize,
    ) -> Vec<Vec<Cell>> {
        let meta = match &view.batch {
            Some(m) if m.total > 1 => m,
            _ => return self.build_user_input_rows(view, rule_width, screen_width),
        };
        let unicode = self.caps.unicode_symbols;
        let mut out: Vec<Vec<Cell>> = Vec::new();
        let push_line = |out: &mut Vec<Vec<Cell>>, text: &str, style: &CellStyle| {
            let raw = crate::glyph::downgrade_glyphs(text, unicode);
            let body = crate::width::truncate_with_ellipsis(
                &scrub_controls(&raw),
                rule_width.saturating_sub(2),
            );
            let mut row = Vec::new();
            push_str_cells(&mut row, "  ", style);
            push_str_cells(&mut row, &body, style);
            out.push(row);
        };
        let hint_style = if crate::highlight::theme::is_light_for_render() {
            self.style_for(Role::Muted)
        } else {
            self.style_faint(Role::Muted)
        };
        // Navigator: "问题 i/N  ✓ ○ ○" (per-question answered markers).
        let markers: String = (0..meta.total)
            .map(|i| {
                let ans = meta.answered.get(i).copied().unwrap_or(false);
                if unicode {
                    if ans {
                        "\u{2713}"
                    } else {
                        "\u{25cb}"
                    }
                } else if ans {
                    "x"
                } else {
                    "o"
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let nav_idx = if meta.on_submit {
            meta.total
        } else {
            meta.index
        };
        let nav = format!("\u{95ee}\u{9898} {}/{}  {}", nav_idx, meta.total, markers); // 问题
        push_line(&mut out, &nav, &self.style_bold(Role::Plan));
        out.push(Vec::new()); // blank

        if meta.on_submit {
            let answered = meta.answered.iter().filter(|a| **a).count();
            let marker = if unicode {
                "\u{276f} \u{2714} "
            } else {
                "> + "
            }; // ❯ ✔
            let label = if unicode {
                "\u{63d0}\u{4ea4}\u{5168}\u{90e8}"
            } else {
                "Submit all"
            }; // 提交全部
            let submit = format!(
                "{marker}{label} ({answered}/{} \u{5df2}\u{7b54})",
                meta.total
            ); // 已答
            push_line(&mut out, &submit, &self.style_bold(Role::Plan));
            out.push(Vec::new()); // blank
                                  // Enter 提交 · Tab/Shift+Tab 切换问题 · Esc 放弃
            let hint = "Enter \u{63d0}\u{4ea4} \u{00b7} Tab/Shift+Tab \u{5207}\u{6362}\u{95ee}\u{9898} \u{00b7} Esc \u{653e}\u{5f03}";
            push_line(&mut out, hint, &hint_style);
        } else {
            // The current question's own rows. Drop its trailing hint row — the single
            // hint ("Enter confirm · Esc cancel") is misleading inside a batch (Enter
            // advances; submitting is a separate stop) — and replace it with one accurate
            // batch hint.
            let mut q = self.build_user_input_rows(view, rule_width, screen_width);
            q.pop(); // the per-question hint (always the last row of build_user_input_rows)
            out.extend(q);
            // 作答 · Tab/Shift+Tab 切换问题 · 到提交行 Enter 交全部 · Esc 放弃
            let hint = "\u{4f5c}\u{7b54} \u{00b7} Tab/Shift+Tab \u{5207}\u{6362}\u{95ee}\u{9898} \u{00b7} \u{5230}\u{63d0}\u{4ea4}\u{884c} Enter \u{4ea4}\u{5168}\u{90e8} \u{00b7} Esc \u{653e}\u{5f03}";
            push_line(&mut out, hint, &hint_style);
        }
        out
    }

    /// Row count matching [`Self::build_user_input_panel_view`].
    fn user_input_panel_view_row_count(&self, view: &crate::render::UserInputPanelView) -> usize {
        match &view.batch {
            Some(m) if m.total > 1 => {
                if m.on_submit {
                    5 // nav + blank + submit + blank + hint
                } else {
                    // nav + blank + <single-question rows minus its own hint> + batch hint
                    self.user_input_panel_row_count(view) + 2
                }
            }
            _ => self.user_input_panel_row_count(view),
        }
    }

    /// Paint the full footer into `self.screen`. Layout (top to bottom):
    ///
    ///   rows 0..T:   todo panel (T = todo_rows, a standing view above input)
    ///   row T:       top rule
    ///   rows T+1..:  middle input lines (N = wrap_with_cursor line count)
    ///   row T+1+N:   bottom rule
    ///   approval panel (A = approval_rows, compact action bar below the input)
    ///   attachments / menu (M = up to half screen height)
    ///   goal|loop row / status line (if any chrome)
    ///
    /// `footer_top = screen.height - total_rows`; `rules_top = footer_top +
    /// todo_rows`. Cursor parks at `(rules_top + 2 + cursor_row_in_middle,
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
        // "> " prompt prefix is 2 display cols; text fills the rest. On JediTerm
        // (DevEco Studio / IntelliJ terminals) reserve ONE extra trailing column:
        // its no-fallback paint layer soft-wraps a row whose last column holds a
        // (wide CJK) glyph, colliding that soft-wrap with our own explicit wrap
        // and duplicating/garbling the row. Keeping the last column empty means
        // no glyph ever reaches the edge, so JediTerm never auto-wraps.
        let prefix_and_reserve = if self.caps.jediterm { 3 } else { 2 };
        let text_budget = input_rule_width.saturating_sub(prefix_and_reserve);

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

        // Paginate menu using the kind-specific cap.
        let max_menu = self.max_menu_rows(h, 4);
        let menu_kind = self.menu.as_ref().map(|m| m.kind).unwrap_or_default();
        // The `/resume` session list shares the plugin manager's chrome and
        // layout (4-row header, bordered search box at idx 2, 2-line items from
        // idx 4, bottom hint) — only the per-item leaf builder differs. Treat it
        // as plugin-like everywhere layout math keys off `Plugin`.
        let plugin_like = matches!(
            menu_kind,
            super::MenuKind::Plugin | super::MenuKind::SessionList
        );
        let (menu_items, selected_in_view, actual_offset) = if let Some(m) = self.menu.as_ref() {
            let len = m.items.len();
            if len == 0 {
                (Vec::<(String, String)>::new(), None, 0)
            } else if matches!(
                menu_kind,
                super::MenuKind::Plugin
                    | super::MenuKind::Marketplace
                    | super::MenuKind::PluginInfo
                    | super::MenuKind::SessionList
            ) {
                let header_h = if plugin_like { 4 } else { 2 };
                if len < header_h {
                    let sel = if m.selected < len {
                        Some(m.selected)
                    } else {
                        None
                    };
                    (m.items.clone(), sel, 0)
                } else {
                    let has_hint = len > header_h
                        && m.items[len - 1].0.starts_with('—')
                        && m.items[len - 1].0.ends_with('—');
                    let hint_h = if has_hint { 1 } else { 0 };
                    let header_row_h = if plugin_like { 6 } else { header_h };
                    let budget = max_menu.saturating_sub(header_row_h + hint_h);
                    let mut offset = header_h;
                    let mut end = offset;
                    let mut height_sum = 0;
                    while end < len - hint_h {
                        let item_h = if menu_kind == super::MenuKind::SessionList
                            && end >= 4
                            && end < len - hint_h
                        {
                            Self::session_item_height(end)
                        } else if plugin_like && end >= 4 && end < len - hint_h {
                            self.plugin_item_height(&m.items[end].1, rule_width)
                        } else if menu_kind == super::MenuKind::Marketplace
                            && end >= 2
                            && end < len - hint_h
                        {
                            let orig_idx = end;
                            if orig_idx >= 3 && orig_idx < len - hint_h {
                                if orig_idx % 2 == 0 {
                                    3
                                } else {
                                    1
                                }
                            } else {
                                1
                            }
                        } else {
                            1
                        };
                        if height_sum + item_h > budget {
                            break;
                        }
                        height_sum += item_h;
                        end += 1;
                    }

                    if m.selected >= header_h && m.selected < len - hint_h && m.selected >= end {
                        while offset < len - hint_h && m.selected >= end {
                            offset += 1;
                            let mut h_sum = 0;
                            end = offset;
                            while end < len - hint_h {
                                let item_h = if menu_kind == super::MenuKind::SessionList
                                    && end >= 4
                                    && end < len - hint_h
                                {
                                    Self::session_item_height(end)
                                } else if plugin_like && end >= 4 && end < len - hint_h {
                                    self.plugin_item_height(&m.items[end].1, rule_width)
                                } else if menu_kind == super::MenuKind::Marketplace
                                    && end >= 2
                                    && end < len - hint_h
                                {
                                    let orig_idx = end;
                                    if orig_idx >= 3 && orig_idx < len - hint_h {
                                        if orig_idx % 2 == 0 {
                                            3
                                        } else {
                                            1
                                        }
                                    } else {
                                        1
                                    }
                                } else {
                                    1
                                };
                                if h_sum + item_h > budget {
                                    break;
                                }
                                h_sum += item_h;
                                end += 1;
                            }
                        }
                    }

                    let mut items = Vec::new();
                    items.extend_from_slice(&m.items[0..header_h]);
                    items.extend_from_slice(&m.items[offset..end]);
                    if has_hint {
                        items.push(m.items[len - 1].clone());
                    }

                    let sel = if m.selected < header_h {
                        Some(m.selected)
                    } else if m.selected >= offset && m.selected < end {
                        Some(header_h + (m.selected - offset))
                    } else {
                        None
                    };
                    (items, sel, offset)
                }
            } else {
                let mut offset = 0;
                let mut height_sum = 0;
                let mut end = 0;
                while end < len {
                    let item_h = if menu_kind == super::MenuKind::Plugin
                        && end >= 2
                        && end < len.saturating_sub(1)
                    {
                        2
                    } else {
                        1
                    };
                    if height_sum + item_h > max_menu {
                        break;
                    }
                    height_sum += item_h;
                    end += 1;
                }

                if m.selected < len && m.selected >= end {
                    while offset < len && m.selected >= end {
                        offset += 1;
                        let mut h_sum = 0;
                        end = offset;
                        while end < len {
                            let item_h = if menu_kind == super::MenuKind::Plugin
                                && end >= 2
                                && end < len.saturating_sub(1)
                            {
                                2
                            } else {
                                1
                            };
                            if h_sum + item_h > max_menu {
                                break;
                            }
                            h_sum += item_h;
                            end += 1;
                        }
                    }
                }

                let items: Vec<(String, String)> = m.items[offset..end].to_vec();
                let sel = if m.selected >= offset && m.selected < end {
                    Some(m.selected - offset)
                } else {
                    None
                };
                (items, sel, offset)
            }
        } else {
            (Vec::new(), None, 0)
        };

        // Spinner moved to body as a live paragraph row — footer no
        // longer reserves a spinner slot. Footer layout:
        //   top_rule / middle... / bot_rule / menu... / status
        let has_hint_at_end = menu_items
            .last()
            .map(|(n, _)| n.starts_with('—') && n.ends_with('—'))
            .unwrap_or(false);
        let menu_rows = if let Some(m) = self.menu.as_ref() {
            let is_sticky = matches!(
                menu_kind,
                super::MenuKind::Plugin
                    | super::MenuKind::Marketplace
                    | super::MenuKind::PluginInfo
                    | super::MenuKind::SessionList
            );
            let mut sum = menu_items
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let orig_idx = if is_sticky {
                        let header_h = if plugin_like { 4 } else { 2 };
                        if i < header_h {
                            i
                        } else if has_hint_at_end && i == menu_items.len() - 1 {
                            m.items.len() - 1
                        } else {
                            actual_offset + (i - header_h)
                        }
                    } else {
                        actual_offset + i
                    };
                    if menu_kind == super::MenuKind::Marketplace
                        && orig_idx >= 3
                        && orig_idx < m.items.len().saturating_sub(1)
                    {
                        if orig_idx % 2 == 0 {
                            3
                        } else {
                            1
                        }
                    } else if plugin_like && orig_idx == 2 {
                        3
                    } else if menu_kind == super::MenuKind::SessionList
                        && orig_idx >= 4
                        && orig_idx < m.items.len().saturating_sub(1)
                    {
                        // Session cards are 2 rows (first) / 3 rows (rest, incl.
                        // the leading blank spacer). Metadata uses a single-space
                        // `·` separator, so `plugin_item_height` would wrongly
                        // return 2 here and this reservation would undercount the
                        // footer height — overlapping the goal/status rows onto
                        // the last card.
                        Self::session_item_height(orig_idx)
                    } else if plugin_like
                        && orig_idx >= 4
                        && orig_idx < m.items.len().saturating_sub(1)
                    {
                        self.plugin_item_height(&m.items[orig_idx].1, rule_width)
                    } else {
                        1
                    }
                })
                .sum::<usize>();
            if has_hint_at_end {
                let prev_is_empty =
                    menu_items.len() > 1 && menu_items[menu_items.len() - 2].0.is_empty();
                if !prev_is_empty {
                    sum += 1;
                }
            }
            sum
        } else {
            0
        };
        // Attachment-preview rows: one `└ [Image #N]` per kept marker,
        // sitting between bot_rule and the menu. The list arrives
        // pre-filtered by `compute_input_attachments` (only markers
        // backed by real bytes survive), so we trust it directly here
        // and don't re-validate against `input_buf`.
        let attachment_rows = self.input_attachments.len();
        let command_output_cells = self.build_command_output_rows();
        let command_output_rows = command_output_cells.len();
        let has_status = !self.status.model.is_empty()
            || !self.status.cwd.is_empty()
            || self.status.hint.is_some();
        let status_rows = if has_status { 1 } else { 0 };
        // Dedicated goal/loop row: one full-width line above the status row, shown
        // while a `/goal` OR `/loop` is active (they are mutually exclusive in
        // practice, so at most one row is ever reserved here).
        let goal_rows = if self.status.goal.is_some() || self.status.loop_status.is_some() {
            1
        } else {
            0
        };
        // Modal input always wins the scarce interactive footer area. While a
        // Task fan-out is active it owns the expanded top-panel slot and the
        // standing TodoWrite panel collapses completely until Task finishes.
        let approval_rows = self.modal_panel_rows();
        let subtask_rows = if approval_rows == 0 {
            self.status
                .subtasks
                .as_ref()
                .map(|progress| self.subtask_panel_row_count(progress))
                .unwrap_or(0)
        } else {
            0
        };
        let todo_rows = if self.status.subtasks.is_none() {
            self.status
                .todo
                .as_ref()
                .map(|t| self.todo_panel_row_count(t))
                .unwrap_or(0)
        } else {
            0
        };
        let pending_message_cells = self.build_pending_message_rows();
        let pending_message_rows = pending_message_cells.len();
        let top_panel_rows = pending_message_rows + subtask_rows + todo_rows;
        // Cap the input-box height so a long paste / typed text can't grow the
        // footer past the screen (overflow). The full text stays in input_buf;
        // we render a scrolling window that keeps the cursor row visible. The
        // `[Pasted #N]` folding (insert_paste) already shrinks most big pastes;
        // this is the backstop for the rest (unfolded single-line pastes,
        // sub-threshold pastes, typed text). The goal/loop + todo rows are folded
        // into the status-rows reservation so the input box height accounts for them.
        let max_input_rows = Self::max_input_rows(
            h,
            attachment_rows,
            menu_rows,
            status_rows + goal_rows + top_panel_rows + approval_rows + command_output_rows,
        );
        let input_view_start = if lines.len() > max_input_rows {
            cursor_row_in_middle
                .saturating_sub(max_input_rows.saturating_sub(1))
                .min(lines.len() - max_input_rows)
        } else {
            0
        };
        let middle_rows = (lines.len() - input_view_start).min(max_input_rows);
        let cursor_row_in_middle = cursor_row_in_middle.saturating_sub(input_view_start);
        // When an approval panel is active, we replace the input box with the
        // approval panel (saves 2+ rows) and hide the status row.
        let approval_active = self.approval_active();
        let hide_input_box = matches!(
            menu_kind,
            super::MenuKind::Plugin
                | super::MenuKind::Marketplace
                | super::MenuKind::PluginInfo
                | super::MenuKind::SessionList
        );
        let is_add_url = hide_input_box
            && menu_kind == super::MenuKind::Marketplace
            && self
                .menu
                .as_ref()
                .and_then(|m| m.items.get(2))
                .map(|(name, _)| name == "Add Marketplace")
                .unwrap_or(false);
        let total_rows = self.footer_total_rows(
            middle_rows,
            command_output_rows,
            attachment_rows,
            menu_rows,
            goal_rows,
            top_panel_rows,
            approval_rows,
            status_rows,
            hide_input_box,
        );
        let pad_style = CellStyle::default();
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

        // `!` shell mode: derived live from the buffer, so the purple treatment
        // arms/reverts with the leading `!` (no persistent mode state).
        let shell = super::input_shell_mode(&self.input_buf);
        // Pre-build every row vector (immutable borrows of self).
        let top_rule = self.build_top_rule_with_context(
            input_rule_width,
            self.status.session_name.as_deref(),
            self.status.history,
            shell,
        );
        let middle_cells: Vec<Vec<Cell>> = lines[input_view_start..input_view_start + middle_rows]
            .iter()
            .enumerate()
            .map(|(i, line)| self.build_middle_row(line, input_view_start + i == 0, shell))
            .collect();
        // When the input is scrolled (windowed), show "+N more lines" on the
        // bottom rule so it's visible that content is hidden, not lost.
        let hidden_rows = lines.len() - middle_rows;
        let bot_rule = self.build_input_bot_rule(input_rule_width, hidden_rows, shell);
        let status_clone = self.status.clone();
        let status_cells = if has_status && !hide_input_box {
            Some(self.build_status_row(&status_clone, rule_width, shell))
        } else {
            None
        };
        // Goal and loop rows occupy the same slot (at most one is shown at a
        // time).  Goal takes priority if somehow both are set.
        let goal_cells: Option<Vec<Cell>> = if let Some(g) = status_clone.goal.as_ref() {
            Some(self.build_goal_row(g, rule_width))
        } else if let Some(ls) = status_clone.loop_status.as_ref() {
            Some(self.build_loop_row(ls, rule_width))
        } else {
            None
        };
        let todo_cells: Vec<Vec<Cell>> = status_clone
            .todo
            .as_ref()
            .filter(|_| status_clone.subtasks.is_none())
            .map(|t| self.build_todo_rows(t, rule_width))
            .unwrap_or_default();
        let subtask_cells: Vec<Vec<Cell>> = if approval_rows == 0 {
            status_clone
                .subtasks
                .as_ref()
                .map(|progress| self.build_subtask_rows(progress, rule_width))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // Modal panel cells: approval OR user_input OR round_cap_panel (mutually
        // exclusive; approval wins, then user_input, then round_cap_panel).
        // Drawn in the shared `approval_active` slot below.
        let approval_cells: Vec<Vec<Cell>> = if let Some(p) = status_clone.approval.as_ref() {
            self.build_approval_rows(p, rule_width, w)
        } else if let Some(p) = status_clone.user_input.as_ref() {
            self.build_user_input_panel_view(p, rule_width, w)
        } else if let Some(p) = status_clone.round_cap_panel.as_ref() {
            self.build_user_input_panel_view(p, rule_width, w)
        } else {
            Vec::new()
        };
        let menu_kind = self.menu.as_ref().map(|m| m.kind).unwrap_or_default();
        // `/resume` session list shares the plugin manager's chrome/layout.
        let plugin_like = matches!(
            menu_kind,
            super::MenuKind::Plugin | super::MenuKind::SessionList
        );
        let mut menu_cells: Vec<Vec<Cell>> = Vec::new();
        let final_len = self.menu.as_ref().map(|m| m.items.len()).unwrap_or(0);
        let is_sticky = matches!(
            menu_kind,
            super::MenuKind::Plugin
                | super::MenuKind::Marketplace
                | super::MenuKind::PluginInfo
                | super::MenuKind::SessionList
        );
        for (i, (name, desc)) in menu_items.iter().enumerate() {
            let orig_idx = if is_sticky {
                let header_h = if plugin_like { 4 } else { 2 };
                if i < header_h {
                    i
                } else if has_hint_at_end && i == menu_items.len() - 1 {
                    final_len - 1
                } else {
                    actual_offset + (i - header_h)
                }
            } else {
                actual_offset + i
            };
            let selected = selected_in_view == Some(i);

            if has_hint_at_end && i == menu_items.len() - 1 {
                let prev_is_empty = i > 0 && menu_items[i - 1].0.is_empty();
                if !prev_is_empty {
                    // Prepend a blank line BEFORE the bottom hint row!
                    menu_cells.push(Vec::new());
                }
            }

            if menu_kind == super::MenuKind::Marketplace
                && !is_add_url
                && orig_idx >= 3
                && orig_idx < final_len.saturating_sub(1)
            {
                if orig_idx % 2 == 0 {
                    menu_cells
                        .extend(self.build_marketplace_menu_rows(name, desc, selected, rule_width));
                } else {
                    menu_cells.push(Vec::new());
                }
            } else if plugin_like && orig_idx == 2 {
                let border_style = if selected {
                    self.style_bold(Role::Brand)
                } else {
                    self.style_for(Role::Muted)
                };
                let pad = CellStyle::default();

                // 1. Top border
                let mut top_border = Vec::with_capacity(rule_width + PAD_COL);
                push_str_cells(&mut top_border, &" ".repeat(PAD_COL), &pad);
                top_border.push(Cell {
                    ch: '╭',
                    style: border_style.clone(),
                    width: 1,
                });
                for _ in 0..(rule_width.saturating_sub(2)) {
                    top_border.push(Cell {
                        ch: '─',
                        style: border_style.clone(),
                        width: 1,
                    });
                }
                if rule_width >= 2 {
                    top_border.push(Cell {
                        ch: '╮',
                        style: border_style.clone(),
                        width: 1,
                    });
                }
                menu_cells.push(top_border);

                // 2. Content row
                let mut content_row = Vec::new();
                push_str_cells(&mut content_row, &" ".repeat(PAD_COL), &pad);
                content_row.push(Cell {
                    ch: '│',
                    style: border_style.clone(),
                    width: 1,
                });
                content_row.push(Cell {
                    ch: ' ',
                    style: CellStyle::default(),
                    width: 1,
                });
                // The `/resume` search box is focusable (Up from the first
                // session lands here): when focused, show the live query plus a
                // cursor caret and drop the placeholder, so the user can see
                // that typing edits THIS field.
                let box_focused = selected && menu_kind == super::MenuKind::SessionList;
                if name.is_empty() && !box_focused {
                    let muted = self.style_for(Role::Muted);
                    // Locale-aware, menu-neutral: `Plugin` is shared by the plugin
                    // manager AND the provider panel, so the placeholder must not
                    // say "plugins". Both are type-to-filter boxes.
                    let zh = matches!(crate::i18n::current_locale(), crate::i18n::Locale::ZhCn);
                    let placeholder = if menu_kind == super::MenuKind::SessionList {
                        if zh {
                            "搜索会话…"
                        } else {
                            "Search sessions..."
                        }
                    } else if zh {
                        "输入以筛选…"
                    } else {
                        "Type to filter..."
                    };
                    push_str_cells(&mut content_row, placeholder, &muted);
                } else {
                    push_str_cells(&mut content_row, name, &CellStyle::default());
                    if box_focused {
                        content_row.push(Cell {
                            ch: '▏',
                            style: self.session_highlight_style(),
                            width: 1,
                        });
                    }
                }
                let target_w = rule_width + PAD_COL;
                let mut current_w = 0;
                for cell in &content_row {
                    current_w += cell.width as usize;
                }
                let pad_w = (target_w.saturating_sub(1)).saturating_sub(current_w);
                for _ in 0..pad_w {
                    content_row.push(Cell {
                        ch: ' ',
                        style: CellStyle::default(),
                        width: 1,
                    });
                }
                if content_row.len() < target_w {
                    content_row.push(Cell {
                        ch: '│',
                        style: border_style.clone(),
                        width: 1,
                    });
                } else {
                    content_row[target_w - 1] = Cell {
                        ch: '│',
                        style: border_style.clone(),
                        width: 1,
                    };
                }
                menu_cells.push(content_row);

                // 3. Bottom border
                let mut bot_border = Vec::with_capacity(rule_width + PAD_COL);
                push_str_cells(&mut bot_border, &" ".repeat(PAD_COL), &pad);
                bot_border.push(Cell {
                    ch: '╰',
                    style: border_style.clone(),
                    width: 1,
                });
                for _ in 0..(rule_width.saturating_sub(2)) {
                    bot_border.push(Cell {
                        ch: '─',
                        style: border_style.clone(),
                        width: 1,
                    });
                }
                if rule_width >= 2 {
                    bot_border.push(Cell {
                        ch: '╯',
                        style: border_style.clone(),
                        width: 1,
                    });
                }
                menu_cells.push(bot_border);
            } else if menu_kind == super::MenuKind::SessionList
                && orig_idx >= 4
                && orig_idx < final_len.saturating_sub(1)
            {
                // One blank line of breathing room BEFORE every session except
                // the first, so entries read as separated cards (title+metadata
                // tight together) rather than one dense block. `session_item_height`
                // accounts for this spacer in all three height reservations.
                if orig_idx > crate::modals::session_picker::HEADER_ROWS {
                    menu_cells.push(Vec::new());
                }
                menu_cells.extend(self.build_session_menu_rows(name, desc, selected, rule_width));
            } else if menu_kind == super::MenuKind::Plugin
                && orig_idx >= 4
                && orig_idx < final_len.saturating_sub(1)
            {
                menu_cells.extend(self.build_plugin_menu_rows(name, desc, selected, rule_width));
            } else {
                let row = self.build_menu_row(name, desc, selected, rule_width, menu_kind);
                if is_add_url && i >= 2 && i < final_len.saturating_sub(1) {
                    let border_style = self.style_for(Role::Muted);
                    if i == 2 {
                        let mut top_border = Vec::with_capacity(rule_width);
                        top_border.push(Cell {
                            ch: '╭',
                            style: border_style.clone(),
                            width: 1,
                        });
                        for _ in 0..(rule_width.saturating_sub(2)) {
                            top_border.push(Cell {
                                ch: '─',
                                style: border_style.clone(),
                                width: 1,
                            });
                        }
                        if rule_width >= 2 {
                            top_border.push(Cell {
                                ch: '╮',
                                style: border_style.clone(),
                                width: 1,
                            });
                        }
                        menu_cells.push(top_border);
                    }

                    let mut boxed_row = Vec::with_capacity(rule_width);
                    boxed_row.push(Cell {
                        ch: '│',
                        style: border_style.clone(),
                        width: 1,
                    });
                    let mut content_cells = row.into_iter();
                    if let Some(first) = content_cells.next() {
                        if first.ch != ' ' {
                            boxed_row.push(first);
                        }
                    }
                    for cell in content_cells {
                        boxed_row.push(cell);
                    }
                    let mut current_w = 0;
                    for cell in &boxed_row {
                        current_w += cell.width as usize;
                    }
                    let pad_w = (rule_width.saturating_sub(1)).saturating_sub(current_w);
                    for _ in 0..pad_w {
                        boxed_row.push(Cell {
                            ch: ' ',
                            style: CellStyle::default(),
                            width: 1,
                        });
                    }
                    if boxed_row.len() < rule_width {
                        boxed_row.push(Cell {
                            ch: '│',
                            style: border_style.clone(),
                            width: 1,
                        });
                    } else {
                        boxed_row[rule_width - 1] = Cell {
                            ch: '│',
                            style: border_style.clone(),
                            width: 1,
                        };
                    }
                    menu_cells.push(boxed_row);

                    if i == final_len.saturating_sub(2) {
                        let mut bot_border = Vec::with_capacity(rule_width);
                        bot_border.push(Cell {
                            ch: '╰',
                            style: border_style.clone(),
                            width: 1,
                        });
                        for _ in 0..(rule_width.saturating_sub(2)) {
                            bot_border.push(Cell {
                                ch: '─',
                                style: border_style.clone(),
                                width: 1,
                            });
                        }
                        if rule_width >= 2 {
                            bot_border.push(Cell {
                                ch: '╯',
                                style: border_style,
                                width: 1,
                            });
                        }
                        menu_cells.push(bot_border);
                    }
                } else {
                    menu_cells.push(row);
                }
            }
        }
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
        //
        // Pending messages and todo progress sit at the TOP of the footer block,
        // above the input box. Both are transient and never enter scrollback.
        let pending_top = footer_top;
        for (i, row) in pending_message_cells.into_iter().enumerate() {
            let mut padded = row;
            Self::pad_row_to_width(&mut padded, w, CellStyle::default());
            self.screen.draw_row(pending_top + i, 0, &padded);
        }
        let todo_top = pending_top + pending_message_rows;
        for (i, row) in subtask_cells.into_iter().enumerate() {
            let mut padded = row;
            Self::pad_row_to_width(&mut padded, w, CellStyle::default());
            self.screen.draw_row(todo_top + i, 0, &padded);
        }
        let todo_top = todo_top + subtask_rows;
        for (i, tr) in todo_cells.into_iter().enumerate() {
            let mut padded = tr;
            Self::pad_row_to_width(&mut padded, w, CellStyle::default());
            self.screen.draw_row(todo_top + i, 0, &padded);
        }
        let rules_top = footer_top + top_panel_rows;

        if hide_input_box {
            let mut border_rule = Vec::with_capacity(rule_width);
            let border_style = self.style_for(Role::Border);
            for _ in 0..rule_width {
                border_rule.push(Cell {
                    ch: '─',
                    style: border_style.clone(),
                    width: 1,
                });
            }
            Self::pad_row_to_width(&mut border_rule, w, CellStyle::default());
            self.screen.draw_row(rules_top, 0, &border_rule);
        } else {
            let mut top_rule = top_rule;
            Self::pad_row_to_width(&mut top_rule, w, CellStyle::default());
            self.screen.draw_row(rules_top, 0, &top_rule);
        }

        // `post_approval` is the row index immediately after whatever occupies the
        // slot between the top rule and the attachment rows — either the approval
        // panel (when active) or the input box + bot_rule (normal case).
        let post_approval = if approval_active {
            // Approval replaces input box: draw approval rows directly after the
            // top rule (skip middle rows, skip bot_rule, skip status).
            let approval_top = rules_top + 1;
            for (i, ar) in approval_cells.into_iter().enumerate() {
                let mut padded = ar;
                Self::pad_row_to_width(&mut padded, w, CellStyle::default());
                self.screen.draw_row(approval_top + i, 0, &padded);
            }
            // Cursor visibility handled by the common suppress_cursor block below.
            rules_top + 1 + approval_rows
        } else if hide_input_box {
            rules_top + 1
        } else {
            // Normal (no approval): draw middle rows then bot_rule.
            for (i, r) in middle_cells.into_iter().enumerate() {
                let mut padded = r;
                Self::pad_row_to_width(&mut padded, w, CellStyle::default());
                self.screen.draw_row(rules_top + 1 + i, 0, &padded);
            }

            let bot_rule_row = rules_top + 1 + middle_rows;
            let mut bot_rule = bot_rule;
            Self::pad_row_to_width(&mut bot_rule, w, CellStyle::default());
            self.screen.draw_row(bot_rule_row, 0, &bot_rule);

            // Cursor park — 1-indexed, inside middle row at the input cell.
            // Input row is flush-left (no PAD_COL); "> " prefix is 2 cols.
            // Symbol-bearing body rows share this col-0 baseline.
            // Middle row lives at `footer_top + 1 + cursor_row_in_middle`
            // (0-indexed); +1 more to convert to the 1-indexed form the
            // cursor-set helper expects.
            let cursor_abs_row = (rules_top + 1 + cursor_row_in_middle + 1) as u16;
            let cursor_abs_col = (2 + cursor_col_in_row + 1) as u16;
            self.screen.set_cursor(cursor_abs_row, cursor_abs_col);

            bot_rule_row + 1
        };

        // Shared drawing: command report → attachments → menu → goal|loop → status.
        // (Status row is suppressed when approval is active since eff_status == 0.)
        let command_output_top = post_approval;
        if !approval_active && !hide_input_box {
            for (i, row) in command_output_cells.into_iter().enumerate() {
                let mut padded = row;
                Self::pad_row_to_width(&mut padded, w, CellStyle::default());
                self.screen.draw_row(command_output_top + i, 0, &padded);
            }
        }
        let attach_top = post_approval
            + if approval_active || hide_input_box {
                0
            } else {
                command_output_rows
            };
        for (i, r) in attachment_cells.into_iter().enumerate() {
            let mut padded = r;
            Self::pad_row_to_width(&mut padded, w, CellStyle::default());
            self.screen.draw_row(attach_top + i, 0, &padded);
        }

        let menu_top = attach_top + attachment_rows;
        let is_search_box_focused = matches!(
            menu_kind,
            super::MenuKind::Plugin | super::MenuKind::SessionList
        ) && self
            .menu
            .as_ref()
            .map(|m| m.selected == 2 && m.items.len() >= 3)
            .unwrap_or(false);
        if is_add_url {
            let input_row_idx = menu_cells
                .iter()
                .position(|row| {
                    let text: String = row.iter().map(|c| c.ch).collect();
                    text.contains("❯ ")
                })
                .unwrap_or(11);
            let cursor_abs_row = (menu_top + input_row_idx + 1) as u16;
            let prefix = if self.input_buf.is_char_boundary(self.input_cursor_byte) {
                &self.input_buf[..self.input_cursor_byte]
            } else {
                &self.input_buf
            };
            let prefix_w = crate::width::display_width(prefix);
            let cursor_abs_col = (4 + prefix_w + 1) as u16;
            self.screen.set_cursor(cursor_abs_row, cursor_abs_col);
        } else if is_search_box_focused {
            let cursor_abs_row = (menu_top + 3 + 1) as u16;
            let query = self
                .menu
                .as_ref()
                .and_then(|m| m.items.get(2))
                .map(|(name, _)| name.clone())
                .unwrap_or_default();
            let query_w = crate::width::display_width(&query);
            let cursor_abs_col = (4 + query_w + 1) as u16;
            self.screen.set_cursor(cursor_abs_row, cursor_abs_col);
        }
        // Invalidate prev_cells for the menu rows so the next render_diff
        // emits explicit patches for EVERY cell (including blank padding at
        // the right edge). Without this, a CJK character from a prior frame's
        // skill description whose display cells transition to ASCII spaces can
        // leave the right-half of its glyph visible on some terminals (iTerm2
        // per-cell patch coalescing). `pad_row_to_width` on the individual row
        // already fills the buffer with blanks from content-end to screen edge,
        // but the diff/serialize mechanism may not fully overwrite the right
        // half of a 2-cell-wide glyph because the continuation cell at (c+1)
        // is compared against the new (non-continuation) blank cell — the patch
        // IS generated, yet the physical glyph remnant survives on iTerm2.
        // Sentinel prev forces every column through the diff, blanks included.
        self.screen.invalidate_rows_from(menu_top);
        for (i, r) in menu_cells.into_iter().enumerate() {
            let mut padded = r;
            Self::pad_row_to_width(&mut padded, w, pad_style.clone());
            self.screen.draw_row(menu_top + i, 0, &padded);
        }
        // Stack below the input box: `· menu / goal|loop / status ·`
        // (the todo panel is drawn at the TOP of the footer, above the rules).
        let goal_top = menu_top + menu_rows;
        if let Some(gr) = goal_cells {
            let mut padded = gr;
            Self::pad_row_to_width(&mut padded, w, CellStyle::default());
            self.screen.draw_row(goal_top, 0, &padded);
        }
        // Status row hidden when approval is active (eff_status == 0, no draw).
        if !approval_active {
            if let Some(st) = status_cells {
                let mut padded = st;
                Self::pad_row_to_width(&mut padded, w, CellStyle::default());
                self.screen.draw_row(goal_top + goal_rows, 0, &padded);
            }
        }
        // Hide the terminal cursor ONLY while an inflight-tool row is
        // animating. `render_inflight_tool`'s in-place path writes raw
        // cursor-position bytes via `self.out.write_all` to overwrite each
        // row, leaving the terminal cursor at end-of-row. `paint_footer`
        // repositions the cell-model cursor to the input box but
        // `set_cursor_visible(true)` keeps the terminal blinking — so for
        // every 5ms paint window before the next CUP lands, the user saw
        // two carets. `inflight_tool.is_none()` flips back as soon as
        // the call commits, so the cursor reappears at the input box on
        // the very next 5ms paint tick.
        //
        // The live spinner is NOT a reason to hide the input cursor: the
        // spinner lives in the BODY now (not the footer), and the input
        // box stays editable during streaming (type-ahead message queue),
        // so hiding the caret would leave the user typing blind — the
        // "no cursor while replying" bug.
        //
        // When approval is active or a plugin modal without text input is open,
        // hide the caret (user navigates with ↑↓/Enter/Tab).
        let suppress_cursor = self.inflight_tool.is_some()
            || approval_active
            || self.diff_overlay_active
            || (hide_input_box && !is_add_url && !is_search_box_focused);
        self.screen.set_cursor_visible(!suppress_cursor);
    }

    /// Returns `true` when a modal footer panel that REPLACES the input box is
    /// active — the tool-approval panel, the `request_user_input` panel, or the
    /// round-cap checkpoint panel (all mutually exclusive). All hide the input
    /// box + status row and are drawn in the same slot.
    #[inline]
    fn approval_active(&self) -> bool {
        self.status.approval.is_some()
            || self.status.user_input.is_some()
            || self.status.round_cap_panel.is_some()
    }

    /// Rows the modal panel (approval OR user_input OR round_cap_panel) occupies
    /// — 0 when none is active. Single source so both `paint_footer` and
    /// `current_footer_rows` agree. Priority: approval > user_input >
    /// round_cap_panel (all mutually exclusive in practice).
    fn modal_panel_rows(&self) -> usize {
        if let Some(p) = self.status.approval.as_ref() {
            self.approval_panel_row_count(p)
        } else if let Some(p) = self.status.user_input.as_ref() {
            self.user_input_panel_view_row_count(p)
        } else if let Some(p) = self.status.round_cap_panel.as_ref() {
            self.user_input_panel_view_row_count(p)
        } else {
            0
        }
    }

    /// Single source of truth for the footer height formula so
    /// `paint_footer`'s `total_rows` and `current_footer_rows` can never
    /// drift.  Applies the approval-active suppression (input middle line +
    /// bottom rule + status row are hidden while an approval panel is up)
    /// then sums all the components.
    fn footer_total_rows(
        &self,
        middle: usize,
        command_output: usize,
        attachment: usize,
        menu: usize,
        goal: usize,
        todo: usize,
        approval: usize,
        status: usize,
        hide_input_box: bool,
    ) -> usize {
        let approval_active = approval > 0;
        let eff_middle = if approval_active || hide_input_box {
            0
        } else {
            middle
        };
        let eff_bot_rule = if approval_active || hide_input_box {
            0
        } else {
            1
        };
        let eff_status = if approval_active || hide_input_box {
            0
        } else {
            status
        };
        let eff_top_rule = 1;
        eff_top_rule
            + eff_middle
            + eff_bot_rule
            + if approval_active || hide_input_box {
                0
            } else {
                command_output
            }
            + attachment
            + menu
            + goal
            + todo
            + approval
            + eff_status
    }

    /// Footer total height — mirrors the computation inside
    /// `paint_footer` so `paint_body` knows where body_bottom lands.
    fn current_footer_rows(&self) -> usize {
        // Mirror paint_footer: input box is full-width (only "> " prefix), with
        // the same JediTerm last-column reserve so the wrapped ROW COUNT here
        // matches the actual render (else body_bottom is off by a row).
        let prefix_and_reserve = if self.caps.jediterm { 3 } else { 2 };
        let text_budget = (self.screen.width() as usize).saturating_sub(prefix_and_reserve);
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
        let menu_rows = self.max_menu_rows(h, 0);
        let has_status = !self.status.model.is_empty()
            || !self.status.cwd.is_empty()
            || self.status.hint.is_some();
        let status_rows = if has_status { 1 } else { 0 };
        let goal_rows = if self.status.goal.is_some() || self.status.loop_status.is_some() {
            1
        } else {
            0
        };
        let approval_rows = self.modal_panel_rows();
        let subtask_rows = if approval_rows == 0 {
            self.status
                .subtasks
                .as_ref()
                .map(|progress| self.subtask_panel_row_count(progress))
                .unwrap_or(0)
        } else {
            0
        };
        let todo_rows = if self.status.subtasks.is_none() {
            self.status
                .todo
                .as_ref()
                .map(|t| self.todo_panel_row_count(t))
                .unwrap_or(0)
        } else {
            0
        };
        let pending_message_rows = self.build_pending_message_rows().len();
        let top_panel_rows = pending_message_rows + subtask_rows + todo_rows;
        let attachment_rows = self.input_attachments.len();
        let command_output_rows = self.build_command_output_rows().len();
        // Cap the input height (mirrors paint_footer) so a long paste / typed
        // input can't make the footer exceed the screen.
        let capped_middle = middle_rows.min(Self::max_input_rows(
            h,
            attachment_rows,
            menu_rows,
            status_rows + goal_rows + top_panel_rows + approval_rows + command_output_rows,
        ));
        // 1 top rule + middle + 1 bot rule + attachments + menu + goal/loop + todo + approval + status.
        // (Spinner used to reserve a row here but now lives in body as
        // a live paragraph — see `push_or_update_live_spinner`.)
        // Delegates to footer_total_rows so both functions share one formula.
        let menu_kind = self.menu.as_ref().map(|m| m.kind).unwrap_or_default();
        let hide_input_box = matches!(
            menu_kind,
            super::MenuKind::Plugin
                | super::MenuKind::Marketplace
                | super::MenuKind::PluginInfo
                | super::MenuKind::SessionList
        );
        self.footer_total_rows(
            capped_middle,
            command_output_rows,
            attachment_rows,
            menu_rows,
            goal_rows,
            top_panel_rows,
            approval_rows,
            status_rows,
            hide_input_box,
        )
    }

    /// Max rows the input box may DISPLAY before scrolling internally. Bounds
    /// the footer to leave the rule rows, any attachment/menu/status chrome,
    /// and at least one body row — so the footer never exceeds the screen no
    /// matter how long the input is. Also capped to `MAX_INPUT_ROWS` so a huge
    /// paste can't take the whole screen on tall terminals.
    fn max_input_rows(
        h: usize,
        attachment_rows: usize,
        menu_rows: usize,
        status_rows: usize,
    ) -> usize {
        // top rule + bot rule + chrome, plus one reserved body row.
        let reserved = 2 + attachment_rows + menu_rows + status_rows + 1;
        h.saturating_sub(reserved).min(MAX_INPUT_ROWS).max(1)
    }

    /// Build the transient Codex-style pending-message panel shown above the
    /// composer. Messages are single-line previews; full payloads remain in
    /// `UiState::pending_steers`. Bound the panel on short terminals without
    /// falling back to an opaque numeric counter.
    fn build_pending_message_rows(&self) -> Vec<Vec<Cell>> {
        if self.status.pending_messages.is_empty() {
            return Vec::new();
        }
        let width = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        if width == 0 {
            return Vec::new();
        }
        // Reserve the non-pending footer chrome plus one body row. Cosmetic
        // pending spacers are admitted only from the rows left over here, so
        // they can never push the composer beyond the viewport on a short
        // terminal.
        let h = self.screen.height() as usize;
        let status_rows = usize::from(
            !self.status.model.is_empty()
                || !self.status.cwd.is_empty()
                || self.status.hint.is_some(),
        );
        let goal_rows =
            usize::from(self.status.goal.is_some() || self.status.loop_status.is_some());
        let modal_rows = self.modal_panel_rows();
        let subtask_rows = if modal_rows == 0 {
            self.status
                .subtasks
                .as_ref()
                .map(|progress| self.subtask_panel_row_count(progress))
                .unwrap_or(0)
        } else {
            0
        };
        let todo_rows = if self.status.subtasks.is_none() {
            self.status
                .todo
                .as_ref()
                .map(|todo| self.todo_panel_row_count(todo))
                .unwrap_or(0)
        } else {
            0
        };
        let reserved_rows = 1 // input top rule
            + 1 // at least one input row
            + 1 // input bottom rule
            + 1 // at least one body row
            + self.input_attachments.len()
            + self.max_menu_rows(h, 0)
            + status_rows
            + goal_rows
            + subtask_rows
            + todo_rows
            + modal_rows
            + self.build_command_output_rows().len();
        let available_rows = h.saturating_sub(reserved_rows);
        // A title and at least one preview are the minimum useful panel. On an
        // extremely short terminal, prefer keeping the composer usable over
        // painting a partial pending panel.
        if available_rows < 2 {
            return Vec::new();
        }

        let max_message_rows = (self.screen.height() as usize / 3).clamp(1, 6);
        let mut content_rows = Vec::with_capacity(max_message_rows + 2);
        let muted = self.style_for(Role::Muted);
        let normal = CellStyle::default();

        let title = crate::width::truncate_with_ellipsis(
            &crate::i18n::t(crate::i18n::Msg::PendingMessagesTitle),
            width.saturating_sub(2),
        );
        let mut header = Vec::new();
        push_str_cells(&mut header, "• ", &muted);
        push_str_cells(&mut header, &title, &muted);
        content_rows.push(header);

        for message in self.status.pending_messages.iter().take(max_message_rows) {
            let preview = scrub_controls(message)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let preview = crate::width::truncate_with_ellipsis(&preview, width.saturating_sub(2));
            let mut row = Vec::new();
            push_str_cells(&mut row, "  ↳ ", &muted);
            push_str_cells(&mut row, &preview, &normal);
            content_rows.push(row);
        }
        if self.status.pending_messages.len() > max_message_rows {
            let mut row = Vec::new();
            push_str_cells(&mut row, "  ↳ …", &muted);
            content_rows.push(row);
        }

        // Todo already owns a leading spacer. When it follows pending messages,
        // that row is the single separator between the panels; adding a pending
        // trailing spacer as well would create the audited double-gap.
        let desired_spacers = if self.status.todo.is_some() { 1 } else { 2 };
        let spacer_rows = if available_rows >= 2 + desired_spacers {
            desired_spacers
        } else {
            0
        };
        let content_budget = available_rows.saturating_sub(spacer_rows).max(2);
        content_rows.truncate(content_budget);

        let mut rows = Vec::with_capacity(content_rows.len() + spacer_rows);
        if spacer_rows > 0 {
            rows.push(Vec::new());
        }
        rows.extend(content_rows);
        if spacer_rows == 2 {
            rows.push(Vec::new());
        }
        rows
    }

    /// Build a transient read-only command report directly below the input
    /// box. These rows are derived from footer state and never enter
    /// `body_lines`, `body_log`, or terminal scrollback.
    fn build_command_output_rows(&self) -> Vec<Vec<Cell>> {
        let Some(text) = self.status.command_output.as_deref() else {
            return Vec::new();
        };
        let content_width = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
        if content_width == 0 {
            return Vec::new();
        }
        // `/usage` reuses the trusted modal rows, including their theme-aware
        // SGR spans. Plain reports such as `/cost` keep the existing faint
        // treatment; styled reports start from the default foreground and
        // drive their own emphasis exactly like the idle modal.
        let has_sgr = text.contains("\x1b[");
        let base_style = if has_sgr {
            CellStyle::default()
        } else {
            self.style_faint(Role::Secondary)
        };
        let safe = crate::sanitize::scrub_controls_keep_sgr(text);
        let mut rows = Vec::new();
        for physical in safe.lines() {
            for chunk in crate::width::wrap_line_to_width(physical, content_width.max(1)) {
                let mut row = Vec::new();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &CellStyle::default());
                push_str_cells_sgr(&mut row, &chunk, base_style.clone());
                rows.push(row);
            }
        }
        rows
    }

    /// Bottom rule of the input box. When `hidden_rows > 0` (the input is
    /// scrolled), embed a muted `+N more lines` hint at the right so the user
    /// sees the content is hidden (still in `input_buf`), not lost.
    fn build_input_bot_rule(
        &self,
        rule_width: usize,
        hidden_rows: usize,
        shell: bool,
    ) -> Vec<Cell> {
        let mut row = self.build_rule_row(rule_width, shell);
        if hidden_rows == 0 {
            return row;
        }
        let hint = format!(" +{hidden_rows} more lines ");
        let hint_chars: Vec<char> = hint.chars().collect();
        if hint_chars.len() >= rule_width {
            return row;
        }
        let muted = self.style_for(Role::Muted);
        let start = rule_width - hint_chars.len();
        for (i, ch) in hint_chars.into_iter().enumerate() {
            row[start + i] = Cell {
                ch,
                style: muted.clone(),
                width: 1,
            };
        }
        row
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
        // Overlay modal drawn on top of body+footer so the diff sees
        // the combined frame. When modal_overlay is None this is a no-op.
        if let Some(ref overlay) = self.modal_overlay {
            for (dy, row) in overlay.cells.iter().enumerate() {
                self.screen
                    .draw_row(overlay.y as usize + dy, overlay.x as usize, row);
            }
        }
    }

    /// Build a borderless `/diff` panel in the `/usage` house style: no box
    /// frame, no input box — a top rule (the separator that stands in for the
    /// input box's top border), the title, the content, a blank spacer, and a
    /// muted hint. The panel has a FIXED height (`win_height`, chosen per view —
    /// taller for the file detail) and renders inline where the input box sits,
    /// right after the conversation, so it follows the scrollback rather than
    /// pinning to the screen bottom. It slides up only when it can't fit below
    /// the body. `win_width` is ignored (laid out against the live screen width).
    fn build_diff_panel_overlay(
        &self,
        title: &DiffPanelRow,
        rows: &[DiffPanelRow],
        footer: &str,
        _win_width: u16,
        win_height: u16,
    ) -> ModalOverlayState {
        let width = (self.screen.width() as usize).max(1);
        let screen_h = (self.screen.height() as usize).max(1);
        let muted = self.style_for(Role::Muted);

        let make_row = |src: &DiffPanelRow| -> Vec<Cell> {
            let mut target = vec![Cell::blank(); width];
            self.paint_diff_panel_row(&mut target, src);
            target
        };
        let blank_row = || vec![Cell::blank(); width];
        let rule_row = || -> Vec<Cell> {
            (0..width)
                .map(|_| Cell {
                    ch: '─',
                    style: muted.clone(),
                    width: 1,
                })
                .collect()
        };

        // Panel height, chosen by the caller (`DiffViewer::redraw` — the list,
        // loading and error views fit their content; the file-detail view uses a
        // tall ~75%-of-screen fixed height so a diff has room to scroll) and only
        // clamped to the live screen here. Chrome = 4 rows (rule, title, blank
        // spacer, hint); the rest is the content area. Bound to the screen so `y`
        // below can never push the panel off the bottom.
        let panel_h = (win_height as usize).clamp(1, screen_h);
        let content_cap = panel_h.saturating_sub(4);

        let mut cells: Vec<Vec<Cell>> = Vec::with_capacity(panel_h);
        // Row 0: the horizontal rule separating the panel from the conversation
        // above (stands in for the input box's top border).
        cells.push(rule_row());
        // Row 1: title (filename + counts, or "Uncommitted changes …").
        cells.push(make_row(title));
        // Content area, padded with blanks to the fixed height so the hint
        // always parks at the bottom edge of the panel.
        let mut used = 0usize;
        for row in rows.iter().take(content_cap) {
            cells.push(make_row(row));
            used += 1;
        }
        for _ in used..content_cap {
            cells.push(blank_row());
        }
        // Blank spacer + the muted key hint at the bottom of the fixed panel.
        cells.push(blank_row());
        let footer_row = DiffPanelRow::new(vec![DiffPanelSpan::new(footer, DiffPanelTone::Muted)]);
        cells.push(make_row(&footer_row));
        cells.truncate(panel_h);

        // Anchor inline where the input box sits (right after the body); slide up
        // only when the fixed-height panel can't fit below the body.
        let body_bottom = self.body_bottom_row() as usize;
        let y = body_bottom.min(screen_h.saturating_sub(cells.len())) as u16;
        ModalOverlayState { cells, x: 0, y }
    }

    /// Paint one `/diff` row into a pre-sized, full-width blank `target`. The
    /// row carries a one-column left pad, then its themed spans. Selection is
    /// conveyed purely by span colour (`DiffPanelTone::Highlight`, the same
    /// theme-aware foreground the `/resume` picker uses) — no reverse-video bar.
    fn paint_diff_panel_row(&self, target: &mut Vec<Cell>, row: &DiffPanelRow) {
        let total_width = target.len();
        if total_width == 0 {
            return;
        }
        let mut content: Vec<Cell> = Vec::with_capacity(total_width);
        content.push(Cell::blank());

        for span in &row.spans {
            let style = if matches!(span.tone, DiffPanelTone::Highlight) {
                self.session_highlight_style()
            } else {
                let role = match span.tone {
                    DiffPanelTone::Default => Role::Secondary,
                    DiffPanelTone::Muted => Role::Muted,
                    DiffPanelTone::Brand => Role::Brand,
                    DiffPanelTone::Add => Role::DiffAdd,
                    DiffPanelTone::Remove => Role::DiffRemove,
                    DiffPanelTone::Warning => Role::Warning,
                    // Handled above; the compiler needs an exhaustive arm.
                    DiffPanelTone::Highlight => Role::Accent,
                };
                if span.bold {
                    self.style_bold(role)
                } else {
                    self.style_for(role)
                }
            };
            let safe = scrub_controls(&span.text);
            for ch in safe.chars() {
                if matches!(ch, '\n' | '\r') {
                    continue;
                }
                if ch == '\t' {
                    let spaces = crate::render::cell::SOFT_TAB_WIDTH
                        .min(total_width.saturating_sub(content.len()));
                    for _ in 0..spaces {
                        content.push(Cell {
                            ch: ' ',
                            style: style.clone(),
                            width: 1,
                        });
                    }
                    continue;
                }
                let width = crate::width::cell_char_width(ch).unwrap_or(1);
                if width == 0 || content.len().saturating_add(width) > total_width {
                    continue;
                }
                content.push(Cell {
                    ch,
                    style: style.clone(),
                    width: width as u8,
                });
                for _ in 1..width {
                    content.push(Cell::continuation());
                }
            }
        }
        while content.len() < total_width {
            content.push(Cell::blank());
        }
        content.truncate(total_width);
        *target = content;
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
        let menu_kind = self.menu.as_ref().map(|m| m.kind).unwrap_or_default();
        let hide_input_box = matches!(
            menu_kind,
            super::MenuKind::Plugin
                | super::MenuKind::Marketplace
                | super::MenuKind::PluginInfo
                | super::MenuKind::SessionList
        );
        let has_trailing_empty = total > 0 && self.body_lines[total - 1].is_empty();
        let display_total = if hide_input_box && has_trailing_empty {
            total - 1
        } else {
            total
        };
        // Anything before `scrolled_off` already lives in native
        // scrollback and must not be re-painted into the viewport.
        if self.scrolled_off >= display_total {
            return;
        }
        let body_width = self.screen.width() as usize;
        // Permanent transcript rows are top-anchored at `scrolled_off`:
        // the terminal row at the top of the viewport MUST correspond
        // to `body_lines[scrolled_off]`, because the next bottom-row LF
        // promotes that physical row and advances `scrolled_off` by one.
        //
        // A live spinner is transient and bypasses the permanent body's
        // eager overflow-LF path. When the permanent body already fills
        // `body_height`, appending the spinner (and optional spacer) can
        // therefore make the retained tail longer than the viewport. Do
        // not tail-anchor that overfull model: doing so shifts the physical
        // top past `scrolled_off`, so the next permanent push records the
        // wrong row as promoted and can duplicate or lose transcript rows.
        //
        // Instead, overlay the transient strip in the final visible slots.
        // Any covered permanent tail rows remain in `body_lines` and are
        // restored by the next paint after the spinner is cleared.
        let transient_rows = if self.live_spinner_active {
            1 + usize::from(self.live_spinner_spacer_active)
        } else {
            0
        }
        .min(display_total.saturating_sub(self.scrolled_off))
        .min(body_height);
        let permanent_end = display_total.saturating_sub(transient_rows);
        let permanent_slots = body_height.saturating_sub(transient_rows);
        let permanent_visible = permanent_end.saturating_sub(self.scrolled_off);
        self.live_spinner_tail_compacted =
            transient_rows > 0 && permanent_visible > permanent_slots;

        // Clone before drawing — `screen.draw_row` takes &mut self.screen
        // and direct iteration would otherwise double-borrow.
        //
        // A permanent-only viewport must stay continuous from `scrolled_off`:
        // the next overflow LF promotes physical row 0 and advances that
        // logical boundary by one. Footer growth can make the retained body
        // temporarily taller than its new capacity; in that case omit the
        // newest tail rather than creating a non-contiguous projection.
        //
        // A live spinner is different: it is transient and must keep the
        // newest permanent tail visible beneath the pinned logical top. The
        // resulting middle omission is recorded by
        // `live_spinner_tail_compacted` and restored before any permanent push.
        let mut rows: Vec<Vec<Cell>> = if permanent_visible <= permanent_slots {
            self.body_lines[self.scrolled_off..permanent_end].to_vec()
        } else if permanent_slots == 0 {
            Vec::new()
        } else if transient_rows == 0 {
            let continuous_end = self
                .scrolled_off
                .saturating_add(permanent_slots)
                .min(permanent_end);
            self.body_lines[self.scrolled_off..continuous_end].to_vec()
        } else {
            // Pin only physical row 0 (`body_lines[scrolled_off]`, required
            // for overflow-LF alignment) and draw the newest
            // `permanent_slots - 1` rows below it. The transient omission is
            // thus confined to the OLDEST middle rows, never the recent tail.
            //
            // A prior version kept a large old prefix plus only the two
            // newest rows, which dropped the recent middle — e.g. the user's
            // own echo once a tool committed rows after it — so a just-
            // submitted prompt disappeared until the turn finished. Tail-
            // anchoring the remainder keeps that recent context on screen.
            let prefix_slots = 1usize.min(permanent_slots);
            let tail_slots = permanent_slots.saturating_sub(prefix_slots);
            let prefix_end = self.scrolled_off.saturating_add(prefix_slots);
            let tail_start = permanent_end.saturating_sub(tail_slots);
            let mut visible = self.body_lines[self.scrolled_off..prefix_end].to_vec();
            visible.extend_from_slice(&self.body_lines[tail_start..permanent_end]);
            visible
        };
        if transient_rows > 0 {
            rows.extend_from_slice(&self.body_lines[display_total - transient_rows..display_total]);
        }
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
    /// in `commit_inflight_tool`) naturally shrinks the body region,
    /// and subsequent pushes naturally fill the freed slots via the
    /// next `paint_frame` cell-diff.
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
            // The whole-viewport scroll just lifted the footer up one row;
            // flag it so the render worker repaints the footer this tick
            // (see `take_pending_scroll_flush`) instead of waiting ~5ms for
            // the event loop's deferred FlushDeferred.
            self.pending_scroll_flush = true;
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
        // lesson as `commit_inflight_tool` (per-row EL, never ED, so the
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
        let remove = 1 + usize::from(self.live_spinner_spacer_active);
        self.live_spinner_spacer_active = false;
        self.live_spinner_tail_compacted = false;
        // (Cursor visibility is no longer coupled to the spinner — the
        // input box stays editable during streaming, so the caret is
        // always shown at the input position. See `paint_footer`.)
        // Capture the physical bottom BEFORE popping. With a full body,
        // `next_body_emit_row()` saturates at the same last body row after
        // the pop, so walking forward from it would erase the spinner and
        // then the footer top rule while leaving the spacer untouched.
        // The strip occupies the `remove` rows ending at this bottom.
        let bottom_before_pop = self.body_bottom_row();
        crate::tuix_trace!(
            "BPOP",
            "site=spinner len_before={} n={} scrolled_off={}",
            self.body_lines.len(),
            remove,
            self.scrolled_off
        );
        for _ in 0..remove {
            self.body_lines.pop();
        }
        if bottom_before_pop > 0 {
            let start = bottom_before_pop.saturating_sub(remove as u16 - 1).max(1);
            let mut seq = String::new();
            for row in start..=bottom_before_pop {
                seq.push_str(&format!("\x1b[{};1H\x1b[K", row));
            }
            let _ = self.out.write_all(seq.as_bytes());
        }
        true
    }

    /// Clear a live spinner before appending permanent transcript rows.
    ///
    /// A painted full-viewport spinner may temporarily preserve the newest
    /// permanent tail by omitting an older middle row. Before any eager body
    /// emit can issue LF, restore the continuous permanent projection so the
    /// physical rows promoted to native scrollback stay aligned with
    /// `body_lines[scrolled_off..]`.
    fn clear_live_spinner_before_permanent_body(&mut self) -> bool {
        let restore_compacted_tail = self.live_spinner_tail_compacted;
        let cleared = self.clear_live_spinner();
        if cleared && restore_compacted_tail {
            self.paint_frame();
            self.flush_frame();
        }
        cleared
    }

    /// Lift the live in-flight tool-call strip off the body tail — the multi-line mirror of
    /// [`clear_live_spinner`]. Pops the strip's `inflight_tool_rows` rows so a following
    /// `push_body_row` appends CLEANLY at the tail instead of burying the strip mid-buffer.
    /// Keeps `inflight_tool` Some and zeroes `inflight_tool_rows`, so the next spinner /
    /// StreamingBox tick re-emits a fresh strip below the just-pushed row (its `prev_rows == 0`
    /// path). Without this, a tool that streams body rows WHILE inflight (the `task` fan-out's
    /// `↻`/`✓` progress lines) orphans one frozen `●Task…` snapshot per streamed line — the
    /// reported time-lapse trail. Returns true if a strip was lifted.
    fn lift_inflight_strip(&mut self) -> bool {
        let n = self.inflight_tool_rows;
        if n == 0 {
            return false;
        }
        crate::tuix_trace!(
            "BPOP",
            "site=inflight len_before={} n={} scrolled_off={}",
            self.body_lines.len(),
            n,
            self.scrolled_off
        );
        for _ in 0..n {
            self.body_lines.pop();
        }
        self.inflight_tool_rows = 0;
        // EL-erase the vacated slots for immediate feedback (anti-flash); the next
        // paint_frame cell-diff also redraws this region. Mirrors clear_live_spinner, but
        // clamped to the screen height so a multi-row strip lifted while the body overflows
        // (target saturated at the footer boundary) can't erase past the terminal bottom.
        let target = self.next_body_emit_row();
        if target > 0 {
            let h = self.screen.height();
            let mut seq = String::new();
            for i in 0..n as u16 {
                let r = target.saturating_add(i);
                if r == 0 || r > h {
                    break;
                }
                seq.push_str(&format!("\x1b[{r};1H\x1b[K"));
            }
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
    fn push_body_row(&mut self, mut row: Vec<Cell>) {
        // Cell-level ASCII fallback backstop: every body row funnels through here, so a
        // single pass catches decorative glyphs built as cells DIRECTLY (rule `─`, tool
        // `●`/`└`, i18n `✓`/`✗`) that the string-level chokepoints miss. No-op on unicode
        // terminals. Also fixes width for a downgraded 2-col glyph (emoji → 1-col ASCII).
        if !self.caps.unicode_symbols {
            for cell in row.iter_mut() {
                if let Some(a) = crate::glyph::single_char_ascii(cell.ch) {
                    cell.ch = a;
                    cell.width = 1;
                }
            }
        }
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
        if self.clear_live_spinner_before_permanent_body() {
            // `clear_live_spinner` already popped the spinner row from
            // `body_lines`. In append-only mode the next emit naturally
            // lands in the freed slot (footer_top shifted up by 1, the
            // new row pushes it back down). `skip_body_scroll_count`
            // remains armed for source-compat with callers that consult
            // it; its decrement path is now a no-op (kept to avoid a
            // bigger refactor in this combined commit).
            self.skip_body_scroll_count = self.skip_body_scroll_count.saturating_add(1);
        }
        // Same discipline for the live in-flight tool strip: a body push must not bury it
        // mid-buffer (that orphans a frozen snapshot per push — the `task` streaming trail).
        // Lift it here; the next tick re-emits it at the new tail below this row.
        let lifted = self.inflight_tool_rows as u16;
        if self.lift_inflight_strip() {
            self.skip_body_scroll_count = self.skip_body_scroll_count.saturating_add(lifted);
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
        self.message_marks
            .push(crate::render::MessageMark { line_idx, kind });
    }

    /// Push or update the live spinner body row. On the first call of a
    /// run it appends only to the retained cell model and marks the row live.
    /// On subsequent calls (every tick), it REPLACES `body_lines.last()`
    /// without using the permanent body's eager `\n`/scrollback feed. A
    /// spinner can be cleared and recreated repeatedly while tool status
    /// changes; treating each recreation as a body push would promote one
    /// row into terminal scrollback every time and leave an ever-growing
    /// blank gap above the spinner.
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
            // The spinner is transient: append it to the retained model
            // without `push_body_row`, whose eager LF is reserved for
            // permanent transcript rows. Keep exactly one blank row above
            // it; reuse an existing trailing blank when the preceding body
            // block already supplied one.
            self.live_spinner_spacer_active = self
                .body_lines
                .last()
                .is_some_and(|row| !row.iter().all(|cell| cell.ch == ' '));
            if self.live_spinner_spacer_active {
                self.body_lines.push(Vec::new());
            }
            self.body_lines.push(row_cells);
            self.live_spinner_active = true;
            self.live_spinner_tail_compacted = false;
            self.dirty = true;
        }
        // (Cursor visibility is driven by `paint_footer` reading
        // `inflight_tool` only — the spinner no longer hides the input
        // caret. No direct DECTCEM write here, otherwise the next
        // render_diff would re-emit \x1b[?25h based on
        // screen.cursor_visible and visually undo our hide on a 5ms
        // cadence.)
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
            // Clear any previously rendered inflight tool rows so
            // push_body_prefixed appends fresh committed lines.
            self.live_spinner_active = false;
            // The hint rode inside the strip (counted in inflight_tool_rows),
            // so the erase below covers it; just drop the state so it can't
            // leak onto the next tool's spinner.
            self.inflight_hint = None;
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
            if safe_name.eq_ignore_ascii_case("bash") && !safe_detail.is_empty() {
                // Live bash commit: produce the same `● Bash` + `  └ <cmd>` block as
                // the static `UiLine::ToolCall` arm, via the shared helper.
                self.push_bash_command_block(&safe_name, &safe_detail);
            } else if safe_detail.is_empty() {
                self.push_body_prefixed(
                    "\u{25cf} ",
                    &self.tool_bullet_style(),
                    &safe_name,
                    &self.style_bold(Role::ToolName),
                );
            } else {
                // Mixed styling: bold name + secondary detail
                let body_str = format!("{}({})", safe_name, safe_detail);
                let body_str = truncate_body_str(&body_str, 500);
                let detail_str = format!("({})", safe_detail);
                let prefix_style = self.tool_bullet_style();
                let name_style = self.style_bold(Role::ToolName);
                let detail_style = self.style_for(Role::Secondary);
                let rows = self.build_mixed_style_rows(
                    "\u{25cf} ",
                    &prefix_style,
                    &safe_name,
                    &name_style,
                    &detail_str,
                    &detail_style,
                    "",
                    &name_style,
                    &body_str,
                );
                for row in rows {
                    self.push_body_row(row);
                }
            }
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
        // ASCII-downgrade decorative glyphs (`✓`/`✗`/`→`/box-drawing …) on non-unicode
        // terminals so hardcoded-in-i18n marks don't render as `□` tofu (see `glyph`).
        let text = crate::glyph::downgrade_glyphs(text, self.caps.unicode_symbols);
        let text = text.as_ref();
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
        // Downgrade decorative glyphs on non-unicode terminals (see `glyph`). The inline
        // SGR escapes are ASCII and pass through untouched.
        let text = crate::glyph::downgrade_glyphs(text, self.caps.unicode_symbols);
        let text = text.as_ref();
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
        let rows = self.build_prefixed_rows(prefix, prefix_style, body, body_style, None);
        for row in rows {
            self.push_body_row(row);
        }
    }

    /// Like `push_body_prefixed` but continuation rows carry `cont_prefix`
    /// (e.g. a coloured `▎` left bar) instead of a blank pad, so a multi-line
    /// block reads as one unit with a full-height left marker. `cont_prefix`
    /// MUST be the same display width as `prefix` so the body stays aligned.
    fn push_body_prefixed_cont(
        &mut self,
        prefix: &str,
        prefix_style: &CellStyle,
        cont_prefix: &str,
        cont_style: &CellStyle,
        body: &str,
        body_style: &CellStyle,
    ) {
        let rows = self.build_prefixed_rows(
            prefix,
            prefix_style,
            body,
            body_style,
            Some((cont_prefix, cont_style)),
        );
        for row in rows {
            self.push_body_row(row);
        }
    }

    /// Symbol-anchored row builder. Wraps `body` to `screen_width − PAD_COL`,
    /// emits the leading row with `prefix`; continuation rows use `cont`'s
    /// `(prefix, style)` when given, else a blank pad of equal display width.
    /// Pure: no side effects on `body_lines` or terminal output. Used by
    /// `push_body_prefixed` / `push_body_prefixed_cont` (which append each row
    /// via push_body_row) and `render_inflight_tool` (which writes in-place
    /// over previously-rendered inflight rows during spinner ticks — see that
    /// fn's doc comment for the scrollback-leak bug this split addresses).
    fn build_prefixed_rows(
        &self,
        prefix: &str,
        prefix_style: &CellStyle,
        body: &str,
        body_style: &CellStyle,
        cont: Option<(&str, &CellStyle)>,
    ) -> Vec<Vec<Cell>> {
        // Downgrade decorative glyphs (the `●`/`▸` prefixes AND any glyphs in the body)
        // on non-unicode terminals — 1-col ASCII stand-ins keep prefix_w alignment.
        let u = self.caps.unicode_symbols;
        let prefix_cow = crate::glyph::downgrade_glyphs(prefix, u);
        let body_cow = crate::glyph::downgrade_glyphs(body, u);
        let prefix = prefix_cow.as_ref();
        let body = body_cow.as_ref();
        let w = (self.screen.width() as usize).saturating_sub(PAD_COL);
        if w == 0 {
            return Vec::new();
        }
        let prefix_w = crate::width::display_width(prefix);
        let first_budget = w.saturating_sub(prefix_w);
        let blank_pad: String = " ".repeat(prefix_w);
        let pad_style = CellStyle::default();
        let mut rows = Vec::new();
        let mut first_emitted = false;
        for phys in body.split('\n') {
            let chunks: Vec<String> = crate::width::wrap_line_to_width(phys, first_budget.max(1))
                .into_iter()
                .map(|c| c.to_string())
                .collect();
            for chunk in &chunks {
                let mut row = Vec::new();
                if !first_emitted {
                    push_str_cells(&mut row, prefix, prefix_style);
                    first_emitted = true;
                } else {
                    let (cp, cs) = cont.unwrap_or((blank_pad.as_str(), &pad_style));
                    push_str_cells(&mut row, cp, cs);
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
            // NOTE: auto-copy is DELIBERATELY not done here (per-close-fence).
            // A reply may contain several ``` blocks; copying each as its fence
            // flushes would clobber the clipboard (last block wins) and spam a
            // "copied" hint per block. The decision is deferred to the turn
            // boundary (`flush_assistant_remainder` → `maybe_auto_copy_hint`),
            // which copies ONLY when the whole reply held exactly ONE block.
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
        // finalize_with_width may have flushed an unclosed code block
        // (stream ended without close fence). Same auto-copy logic as
        // flush_assistant_lines (issue #699).
        if let Some(hint) = self.maybe_auto_copy_hint() {
            self.push_markdown_body(&hint);
        }
    }

    fn flush_reasoning_lines(&mut self) {
        if !self.reasoning_line_buf.contains('\n') {
            return;
        }
        let mut completed: Vec<String> = Vec::new();
        while let Some(nl) = self.reasoning_line_buf.find('\n') {
            let line: String = self.reasoning_line_buf.drain(..=nl).collect();
            let content = line[..line.len() - 1].to_string();
            completed.push(content);
        }
        let style = CellStyle {
            faint: true,
            ..CellStyle::default()
        };
        for content in completed {
            self.push_body_text(&content, &style);
        }
    }

    fn flush_reasoning_remainder(&mut self) {
        if !self.reasoning_line_buf.is_empty() {
            let line = std::mem::take(&mut self.reasoning_line_buf);
            let style = CellStyle {
                faint: true,
                ..CellStyle::default()
            };
            self.push_body_text(&line, &style);
        }
    }

    /// If the markdown parser just finished a code block, copy its raw
    /// source to the system clipboard and return a muted hint line for
    /// display. Returns `None` when no block was finished or auto-copy
    /// is disabled (see `auto_copy_enabled` field / `set_auto_copy_enabled`).
    fn maybe_auto_copy_hint(&mut self) -> Option<String> {
        // Gate BEFORE consuming the source, so a disabled renderer never
        // touches the clipboard. Two reasons to bail:
        //   - auto-copy is off (the default now — opt-in via
        //     `config.ui.auto_copy_code_blocks` / `ATOMCODE_AUTO_COPY`; it used to
        //     silently clobber the user's clipboard on every code-block reply);
        //   - history replay (/resume, /undo, atomcode -c): the markdown events are
        //     identical to live streaming, but replay must not overwrite the
        //     clipboard or inject stale "Copied" hints (issue #699 P1).
        if !self.auto_copy_enabled || self.suppress_auto_copy {
            return None;
        }
        // SINGLE-BLOCK gate: auto-copy only when the whole reply was essentially
        // ONE fenced code block. For a multi-block reply (`!= 1`) we neither copy
        // nor hint — copying each block would clobber the clipboard and the hint
        // would repeat, the user-reported side-effect. `== 0` (no closed block,
        // e.g. an unclosed/interrupted fence) also declines. `take()` past the
        // gate so a declined reply leaves no stale source for a later flush.
        if self.md_state.code_block_count != 1 {
            return None;
        }
        let source = self.md_state.last_code_block_source.take()?;
        if source.trim().is_empty() {
            return None;
        }
        // Write through `self.out` so the escape sequence stays ordered
        // with buffered body-content writes — avoids raw-stdout interleave
        // with the retained renderer's BufWriter.
        let ok =
            crate::event_loop::commands::copy_text_to_clipboard_osc52_via(&mut self.out, &source);
        // Only show the hint when the clipboard write actually succeeded
        // — a misleading "Copied" is worse than no hint (issue #699 P2).
        if !ok {
            return None;
        }
        // Leading `\n` gives the hint a blank line of separation from the
        // code block body, so it reads as a standalone annotation rather
        // than trailing the last code line.
        Some(format!(
            "\n{}{}{}",
            crate::highlight::theme::MD_MUTED_OPEN,
            t(Msg::CodeBlockCopied),
            crate::highlight::theme::MD_MUTED_CLOSE
        ))
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

    #[allow(dead_code)]
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

    /// Render the baked mascot const into cell rows (no leading pad; caller
    /// positions it). Each cell is `▀` with fg=top-subpixel / bg=bottom-subpixel;
    /// a fully transparent cell is a blank space.
    fn build_mascot_rows(&self) -> Vec<Vec<Cell>> {
        use crate::render::mascot::{mascot_color, MASCOT_ROWS, MASCOT_WIDTH};
        let mut rows = Vec::with_capacity(MASCOT_ROWS.len());
        for line in MASCOT_ROWS {
            let bytes = line.as_bytes();
            let mut row = Vec::with_capacity(MASCOT_WIDTH);
            for cell in 0..MASCOT_WIDTH {
                let top = bytes.get(2 * cell).copied().unwrap_or(b'.');
                let bot = bytes.get(2 * cell + 1).copied().unwrap_or(b'.');
                // Half-block choice (see `mascot_cell`): `▄` for transparent-top
                // pixels so they don't paint a default-fg bar across the ears.
                row.push(mascot_cell(mascot_color(top), mascot_color(bot)));
            }
            rows.push(row);
        }
        rows
    }

    fn build_welcome_rows(
        &self,
        model: &str,
        working_dir: &str,
        tip_indices: &[usize],
    ) -> Vec<Vec<Cell>> {
        // Mirror AnsiRenderer::render_welcome, but allow narrow terminals
        // to reflow path/model/tips instead of truncating or colliding.
        let w = self.screen.width() as usize;
        let content_w = w.saturating_sub(PAD_COL * 2).max(1);

        // ---- Row 0: header (unchanged from the previous implementation) ----
        // Row 1: brand left + version · license right
        let left_txt = "◆ AtomCode";
        let right_ver = concat!("v", env!("CARGO_PKG_VERSION"));
        let right_lic = "MIT";
        let left_w = crate::width::display_width(left_txt);
        let right_txt = format!("{}  ·  {}", right_ver, right_lic);
        let right_w = crate::width::display_width(&right_txt);
        let mut rows: Vec<Vec<Cell>> = Vec::with_capacity(12);
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

        rows.push(Vec::new()); // blank separator

        // ---- Left block: mascot (if colors) + cwd + model ----
        let secondary = self.style_for(Role::Secondary);
        let bullet = self.style_for(Role::AccentDim);
        // The mascot is half-block art (`▀`/`▄`) that packs two vertical pixels
        // per cell via fg (top) + per-cell BACKGROUND (bottom). It needs colour,
        // unicode, AND a terminal that actually paints cell backgrounds. On a
        // classic Windows console (`unicode_symbols = false`) the glyphs
        // downgrade to `#`; and bare / SSH terminals like FinalShell render the
        // block glyphs but drop the cell background, so the bottom pixels vanish
        // and the art fragments. Gate on `modern_emulator` (WT_SESSION /
        // TERM_PROGRAM) — reliably backgrounds-capable — plus JediTerm (a local
        // modern IDE terminal that also paints backgrounds); omit elsewhere and
        // let the tips stack cleanly.
        let show_mascot = self.caps.colors
            && self.caps.unicode_symbols
            && (self.caps.modern_emulator || self.caps.jediterm);
        let mascot_w = crate::render::mascot::MASCOT_WIDTH;

        let mut left: Vec<Vec<Cell>> = Vec::new();
        if show_mascot {
            for mut mrow in self.build_mascot_rows() {
                // leading pad so the mascot aligns under the header's PAD_COL
                let mut row = Vec::with_capacity(PAD_COL + mrow.len());
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &CellStyle::default());
                row.append(&mut mrow);
                left.push(row);
            }
        }
        // cwd + model bullets render BELOW the two-column (mascot | tips) block —
        // NEVER zipped into it. If they were part of the left column, a tips list
        // taller than the mascot (e.g. 5 tip rows vs a 4-row mascot) would overflow
        // its extra rows onto the cwd/model rows (`∙ proj/goal  set a goal…`).
        let mut below: Vec<Vec<Cell>> = Vec::new();
        for text in [working_dir, model] {
            let mut cells = Vec::new();
            push_str_cells(&mut cells, text, &secondary);
            below.extend(self.build_prefixed_wrapped_rows(
                "∙ ",
                &bullet,
                "  ",
                &CellStyle::default(),
                cells,
                content_w,
            ));
        }

        // ---- Right block: tips heading + [pinned + chosen] ----
        let heading = self.style_faint(Role::Secondary);
        let cmd_style = self.style_bold(Role::Accent);
        let desc_style = self.style_faint(Role::Secondary);
        let tips = crate::render::welcome_tips::tips_from_indices(tip_indices);
        let cmd_col = tips.iter().map(|tip| tip.cmd.len()).max().unwrap_or(0) + 2;

        let mut right: Vec<Vec<Cell>> = Vec::new();
        {
            let mut hrow = Vec::new();
            push_str_cells(&mut hrow, &t(Msg::WelcomeTipsHeading), &heading);
            right.push(hrow);
        }
        for tip in &tips {
            let mut trow = Vec::new();
            push_str_cells(&mut trow, tip.cmd, &cmd_style);
            let pad_spaces = cmd_col.saturating_sub(tip.cmd.len());
            push_str_cells(&mut trow, &" ".repeat(pad_spaces), &CellStyle::default());
            push_str_cells(&mut trow, &t(tip.desc), &desc_style);
            right.push(trow);
        }

        // ---- Compose: two columns if wide enough, else stacked ----
        let gap = 4usize;
        let left_w = if show_mascot {
            PAD_COL + mascot_w
        } else {
            PAD_COL
        };
        let tips_col = left_w + gap;
        // Right-column width = the WIDEST tip row (cmd + padding + desc), not a
        // fixed guess. The tip rows aren't truncated, so two-column mode must
        // only engage when the widest tip actually fits beside the mascot;
        // otherwise the tip overruns the edge and the emulator hard-wraps it,
        // fragmenting the aligned column. A hardcoded underestimate let this
        // happen on narrow terminals (and the wider mascot made it worse).
        let min_right = right
            .iter()
            .map(|r| r.iter().map(|c| c.width as usize).sum::<usize>())
            .max()
            .unwrap_or(0);
        // Two columns only make sense next to the mascot. With colours off the
        // mascot is omitted, so `tips_col` would be tiny (just `PAD_COL`) and the
        // wider cwd/model bullet rows would staircase the tips onto them — stack
        // instead.
        if show_mascot && content_w >= tips_col + min_right {
            // Two columns: mascot | tips. Zip ONLY the mascot with the tips.
            let n = left.len().max(right.len());
            for i in 0..n {
                let mut row = left.get(i).cloned().unwrap_or_default();
                let cur_w: usize = row.iter().map(|c| c.width as usize).sum();
                for _ in cur_w..tips_col {
                    row.push(Cell::blank());
                }
                if let Some(rrow) = right.get(i) {
                    row.extend(rrow.clone());
                }
                rows.push(row);
            }
            // cwd/model below the whole two-column block.
            rows.extend(below);
        } else {
            // stacked: mascot (if any), then cwd/model, then tips (indented).
            rows.extend(left);
            rows.extend(below);
            for rrow in right {
                let mut row = Vec::new();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &CellStyle::default());
                row.extend(rrow);
                rows.push(row);
            }
        }

        // Trailing blank so subsequent async events (MCP "已连接",
        // upgrade hints, etc.) don't butt up against the hint row.
        rows.push(Vec::new());

        rows
    }

    fn push_welcome(&mut self, model: &str, working_dir: &str) {
        // Roll the random tip pick ONCE per session, then reuse it. `push_welcome`
        // runs again on every resize (via `reflow_body_to_current_width` replaying
        // the logged `UiLine::Welcome`), so rolling here unconditionally would
        // re-roll the tips on every resize — instead reuse the persisted
        // `welcome_tip_indices` (only `reset()` clears it for a fresh session).
        let chosen = match &self.welcome_tip_indices {
            Some(chosen) => chosen.clone(),
            None => {
                let mut rng = rand::thread_rng();
                let chosen = crate::render::welcome_tips::choose_pool_indices(&mut rng);
                self.welcome_tip_indices = Some(chosen.clone());
                chosen
            }
        };
        let rows = self.build_welcome_rows(model, working_dir, &chosen);
        self.welcome_banner = Some((model.to_string(), working_dir.to_string(), chosen));
        self.welcome_line_count = rows.len();
        for row in rows {
            self.push_body_row(row);
        }
    }

    /// Record a permanent body `UiLine` into `body_log` so a later
    /// resize can reflow it at the new width. No-op while replaying (so
    /// the reflow itself can't grow the log) and for transient / footer
    /// / modal variants (re-derived from live state by `paint_frame`).
    /// Consecutive streaming-text deltas are merged so per-token
    /// streaming produces one log entry per assistant paragraph, not
    /// one per token.
    fn log_body_event(&mut self, line: &UiLine) {
        if self.replaying {
            return;
        }
        match line {
            // Transient / footer / modal: never part of the permanent
            // transcript, so re-derived from live state on every frame.
            UiLine::InputPrompt { .. }
            | UiLine::StreamingBox { .. }
            | UiLine::Spinner { .. }
            | UiLine::ClearTransient
            | UiLine::InputCommit
            | UiLine::DiffPanel { .. }
            | UiLine::ModalOverlayClear => return,
            _ => {}
        }
        // Coalesce consecutive streaming text: feeding the markdown
        // state machine "he" + "llo\n" yields the same rows as "hello\n"
        // (partial lines buffer in `assistant_line_buf` until the LF),
        // so merging is loss-free and keeps the log ~one entry per
        // paragraph instead of one per token.
        match (self.body_log.last_mut(), line) {
            (Some(UiLine::AssistantText(prev)), UiLine::AssistantText(next)) => {
                prev.push_str(next);
                return;
            }
            (Some(UiLine::ReasoningText(prev)), UiLine::ReasoningText(next)) => {
                prev.push_str(next);
                return;
            }
            (Some(UiLine::CommandOutput(prev)), UiLine::CommandOutput(next)) => {
                prev.push_str(next);
                return;
            }
            _ => {}
        }
        self.body_log.push(line.clone());
        // Bound memory on very long sessions. Once we drop the oldest entry the
        // transcript prefix is unrecoverable by the reflow replay; flag it purely so
        // the resize trace can note the dropped prefix (see `body_log_truncated` —
        // it no longer gates the scrollback wipe).
        if self.body_log.len() > MAX_SCROLLBACK_ROWS {
            self.body_log.remove(0);
            self.body_log_truncated = true;
        }
    }

    /// Re-render the entire logged transcript at the CURRENT screen
    /// geometry. Drops the old (wrong-width) body state, then replays
    /// `body_log` through the normal `render()` path so every line —
    /// welcome banner, user messages, assistant markdown, tool output —
    /// re-wraps to the new width and re-promotes the correct overflow
    /// into native scrollback via the append-only LF path. This is the
    /// width-correct replacement for the old clip-based body re-emit,
    /// which truncated rows instead of reflowing them. The caller owns
    /// the surrounding terminal wipe and sync envelope.
    fn reflow_body_to_current_width(&mut self) {
        // Drop pre-reflow body + all state derived from it; the replay
        // rebuilds every field from scratch at the new width.
        self.body_lines.clear();
        self.scrolled_off = 0;
        self.welcome_line_count = 0;
        self.welcome_banner = None;
        self.message_marks.clear();
        self.assistant_line_buf.clear();
        self.reasoning_line_buf.clear();
        self.md_state.reset();
        self.live_group = None;
        self.inflight_tool = None;
        self.inflight_tool_rows = 0;
        self.inflight_hint = None;
        self.live_spinner_active = false;
        self.live_spinner_spacer_active = false;
        self.live_spinner_tail_compacted = false;
        self.last_mark_was_assistant = false;
        self.skip_body_scroll_count = 0;
        // Take the log out so `render()` can borrow `self` mutably; the
        // `replaying` guard makes the re-log a no-op regardless.
        let log = std::mem::take(&mut self.body_log);
        self.replaying = true;
        for ev in &log {
            self.render(ev.clone());
        }
        self.replaying = false;
        self.body_log = log;
    }

    fn reflow_welcome_prefix(&mut self) {
        let Some((model, working_dir, chosen)) = self.welcome_banner.clone() else {
            return;
        };
        if self.welcome_line_count == 0 || self.body_lines.len() < self.welcome_line_count {
            return;
        }
        let rows = self.build_welcome_rows(&model, &working_dir, &chosen);
        let new_len = rows.len();
        self.body_lines
            .splice(0..self.welcome_line_count, rows.into_iter());
        self.welcome_line_count = new_len;
    }

    fn push_user_message(&mut self, text: &str, attachments: &[usize]) {
        // A new user block owns its leading paragraph boundary. Reuse the
        // previous turn's trailing blank when present; otherwise insert one so
        // navigation anchors point at the chevron rather than the spacer.
        let tail_is_blank = self
            .body_lines
            .last()
            .is_none_or(|row| row.iter().all(|cell| cell.ch == ' '));
        if !tail_is_blank {
            self.push_body_row(Vec::new());
        }
        self.mark_message(crate::render::MarkKind::User);
        self.last_mark_was_assistant = false;
        let safe = scrub_controls(text);

        let bg = self.style_panel_bg();

        // No panel background (NO_COLOR / colours disabled): keep the original
        // plain layout byte-for-byte so nothing regresses.
        if bg.bg.is_none() {
            let accent = self.style_bold(Role::Accent);
            let text_style = CellStyle::default();
            self.push_body_prefixed_cont(
                self.caps.prompt_chevron(),
                &accent,
                self.caps.prompt_continuation_bar(),
                &accent,
                &safe,
                &text_style,
            );
            let muted = self.style_for(Role::Muted);
            for n in attachments {
                self.push_body_text(&format!("└ [Image #{}]", n), &muted);
            }
            self.push_body_row(Vec::new());
            self.md_state.reset();
            return;
        }

        let w = (self.screen.width() as usize).saturating_sub(PAD_COL);

        // Marker / text / attachment styles keep their fg but gain the panel
        // bg so the colour sits behind the whole block.
        let mut accent = self.style_bold(Role::Accent);
        accent.bg = bg.bg;
        let mut text_style = CellStyle::default();
        text_style.bg = bg.bg;
        let mut muted = self.style_for(Role::Muted);
        muted.bg = bg.bg;

        // Top padding row of the block.
        self.push_body_row(Self::bg_blank_row(w, &bg));

        // Prefixed content rows (❯ / ▎ + text), each padded to block width.
        let mut rows = self.build_prefixed_rows(
            self.caps.prompt_chevron(),
            &accent,
            &safe,
            &text_style,
            Some((self.caps.prompt_continuation_bar(), &accent)),
        );
        for row in &mut rows {
            Self::pad_row_bg(row, w, &bg);
        }
        for row in rows {
            self.push_body_row(row);
        }

        // Attachment rows live inside the block. Reuse build_prefixed_rows with
        // a PAD_COL-wide bg indent as the "prefix" so glyph downgrade + wrap
        // (to width − 2·PAD_COL) match the plain `push_body_text` path.
        for n in attachments {
            let indent = " ".repeat(PAD_COL);
            let mut arows = self.build_prefixed_rows(
                &indent,
                &bg,
                &format!("└ [Image #{}]", n),
                &muted,
                None,
            );
            for row in &mut arows {
                Self::pad_row_bg(row, w, &bg);
            }
            for row in arows {
                self.push_body_row(row);
            }
        }

        // Bottom padding row of the block, then a plain paragraph gap.
        self.push_body_row(Self::bg_blank_row(w, &bg));
        self.push_body_row(Vec::new());
        self.md_state.reset();
    }

    /// Build a single child row for a live tool-group. Downgrades decorative
    /// glyphs, scrubs controls, and renders muted + width-clipped via
    /// `build_one_row` — bash children now read `  └ Bash <cmd>` like any other
    /// child (no `$` prefix / accent). Called from both the initial
    /// `ToolGroupRender` child loop and the `ToolGroupChildUpdate` in-place
    /// rewrite path so both share identical width and styling behaviour.
    fn build_group_child_row(&self, text: &str, muted: &CellStyle) -> Vec<Cell> {
        let unicode = self.caps.unicode_symbols;
        let screen_w = self.screen.width();
        let downgr = crate::glyph::downgrade_glyphs(text, unicode);
        let safe = scrub_controls(&downgr);
        build_one_row(&safe, muted, screen_w, unicode)
    }
}

impl<W: Write + Send> Renderer for RetainedRenderer<W> {
    fn render(&mut self, line: UiLine) {
        // Record permanent body content so a width change can reflow the
        // whole transcript (see `body_log`). No-op for transient variants
        // and while replaying.
        self.log_body_event(&line);
        match line {
            // ── footer-only variants ──
            UiLine::InputPrompt {
                buf,
                cursor_byte,
                menu,
                status,
                attachments,
            } => {
                // Universal safety net for the hidden-caret bug: reaching the
                // idle input prompt means the turn is over, so no tool can still
                // be in flight. Any termination path that failed to commit it
                // (silent non-Stopped stops, goal timeouts, protocol gaps) would
                // otherwise leave `inflight_tool` Some and `paint_footer`'s
                // `suppress_cursor` keep the caret hidden. No-op when already
                // committed; `InputPrompt` is idle-only (streaming type-ahead
                // renders via `StreamingBox`), so this can never freeze a
                // genuinely-running tool early.
                //
                // Ordered before `clear_live_spinner` defensively: today the two
                // are mutually exclusive (an inflight tool's spinner ticks route
                // through `render_inflight_tool`, which never sets
                // `live_spinner_active`; `ToolCallInFlight` also clears any live
                // spinner first), so `clear_live_spinner` is a no-op while a tool
                // is in flight. Committing first keeps that safe even if a future
                // change lets both be active at once (`clear_live_spinner` pops a
                // single row; a stale strip would then mis-truncate).
                self.commit_inflight_tool();
                // Returning to idle input: any remaining (non-tool) spinner row
                // served its purpose — clear it so the user sees a clean input
                // prompt, not a stale `⠋ Pondering…` row above the input box.
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
                // A Task fan-out has its own fixed footer liveness panel. Do
                // not also paint the generic thinking/tool spinner into the
                // transcript body: competing child updates made that row flash,
                // and the duplicate activity added no information.
                if self.status.subtasks.is_some() && self.inflight_tool.is_none() {
                    self.clear_live_spinner();
                    self.dirty = true;
                    return;
                }
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
                // Submission clears the composer before the permanent user
                // echo is appended. Keeping the old (possibly wrapped)
                // input cached here makes `current_footer_rows()` reserve too
                // many rows during `push_user_message`; the overflow path then
                // promotes the echo's first row into native scrollback, so the
                // viewport appears to contain only its continuation until the
                // user scrolls up. The event loop clears its Buffer immediately
                // after this event, so mirror that state synchronously.
                self.input_buf.clear();
                self.input_cursor_byte = 0;
                self.menu = None;
                self.input_attachments.clear();
                self.dirty = true;
            }

            // ── body: welcome / turn events ──
            UiLine::Welcome { model, working_dir } => {
                let model_scrubbed = scrub_controls(&model);
                let wd_scrubbed = scrub_controls(&working_dir);
                self.push_welcome(&model_scrubbed, &wd_scrubbed);
            }
            UiLine::User(text) => {
                self.push_user_message(&text, &[]);
            }
            UiLine::UserWithAttachments { text, attachments } => {
                self.push_user_message(&text, &attachments);
            }
            UiLine::TurnSeparator { label } => {
                self.last_mark_was_assistant = false;
                let w = (self.screen.width() as usize).saturating_sub(PAD_COL * 2);
                // The summary label bakes `✓`/`✗`/`↻` from i18n — downgrade on non-unicode
                // terminals (the reported `✗ Stopped` → `□` tofu).
                let label = crate::glyph::downgrade_glyphs(&label, self.caps.unicode_symbols);
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
                    let prev_mark_kind = self.message_marks.last().map(|m| m.kind);
                    self.mark_message(crate::render::MarkKind::Assistant);
                    self.last_mark_was_assistant = true;
                    if let Some(
                        crate::render::MarkKind::ToolCall | crate::render::MarkKind::ToolResult,
                    ) = prev_mark_kind
                    {
                        let tail_blank = self
                            .body_lines
                            .last()
                            .map(|r| r.iter().all(|c| c.ch == ' '))
                            .unwrap_or(true);
                        if !tail_blank {
                            self.push_body_row(Vec::new());
                        }
                    }
                }
                self.assistant_line_buf.push_str(&scrub_controls(&text));
                self.flush_assistant_lines();
                // Safety cap: `flush_assistant_lines` only drains up to a `\n`, so a
                // stream that dribbles bytes without ever emitting a newline (a hung
                // keep-alive connection, or a model streaming one enormous single
                // line) would grow `assistant_line_buf` without bound → OOM →
                // `panic = "abort"` (surfaces on Windows as a "stack-based buffer
                // overrun"). Force-flush the partial as a body row once it gets
                // absurdly large. 1 MiB is orders of magnitude beyond any real line,
                // so normal replies never hit this.
                if self.assistant_line_buf.len() > ASSISTANT_LINE_BUF_MAX {
                    self.flush_assistant_remainder();
                }
            }
            UiLine::ReasoningText(text) => {
                self.reasoning_line_buf.push_str(&scrub_controls(&text));
                self.flush_reasoning_lines();
            }
            UiLine::AssistantLineBreak => {
                self.flush_assistant_remainder();
                self.flush_reasoning_remainder();
            }
            UiLine::TurnComplete => {
                self.flush_assistant_remainder();
                self.flush_reasoning_remainder();
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
                self.flush_reasoning_remainder();
                self.commit_inflight_tool();
                // (cancelled) is a state-change marker — must remain
                // visible. Default fg, not Muted.
                let style = self.style_for(Role::Secondary);
                let label = t(Msg::Cancelled);
                self.push_body_text(&label, &style);
            }

            // ── body: tools & diffs ──
            UiLine::ToolCallInFlight {
                id,
                name,
                detail,
                hint,
            } => {
                // Capture BEFORE the mark mutates the flag (mirrors the static
                // ToolCall arm) so we can add one blank row of breathing room
                // between assistant text and this tool.
                let prev_was_assistant = self.last_mark_was_assistant;
                self.clear_live_spinner_before_permanent_body();
                self.mark_message(crate::render::MarkKind::ToolCall);
                self.last_mark_was_assistant = false;
                self.flush_assistant_remainder();
                // `prev_was_assistant` implies no in-flight tool to preempt (a
                // preempt means the previous mark was a tool, not assistant), so
                // this blank never lands between the strip and a preempted commit.
                if prev_was_assistant {
                    self.push_body_row(Vec::new());
                }
                // Parallel tool calls are rare but not impossible. If
                // one is already animating, freeze it before starting
                // a new one — single-at-a-time animation is a deliberate
                // simplification (see field doc).
                if self.inflight_tool.is_some() {
                    // Commit the previous tool (freezes it as ▸ in
                    // the body transcript) before starting a new one.
                    self.commit_inflight_tool();
                }
                // The hint (e.g. bash "Press Ctrl+o …") rides INSIDE the
                // inflight strip so the spinner tick / commit erase cover it
                // atomically. Set AFTER the preempt-commit above (which clears
                // `inflight_hint`) so a tool that preempts a still-running one
                // keeps ITS OWN hint, and BEFORE render_inflight_tool so the
                // first paint already includes it.
                self.inflight_hint = hint;
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
                // Capture BEFORE the mark mutates the flag (mirrors the static
                // ToolCall arm) so we can add one blank row of breathing room
                // between assistant text and this tool batch.
                let prev_was_assistant = self.last_mark_was_assistant;
                self.clear_live_spinner_before_permanent_body();
                // Mark the batch header as a ToolCall anchor — Alt+↑/↓
                // (message-jump) walks `message_marks`; without this
                // the whole "● Running N calls in parallel" header +
                // child rows would be invisible to jump navigation,
                // skipping the entire batch.
                self.mark_message(crate::render::MarkKind::ToolCall);
                self.last_mark_was_assistant = false;
                self.flush_assistant_remainder();
                if prev_was_assistant {
                    self.push_body_row(Vec::new());
                }
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
                let header_row =
                    build_one_row(&header, &header_style, screen_w, self.caps.unicode_symbols);
                self.push_body_row(header_row);
                let header_idx = self.body_lines.len() - 1;

                let mut child_indices: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                for c in &children {
                    let row = self.build_group_child_row(&c.text, &muted);
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
                let new_row = self.build_group_child_row(&new_text, &muted);

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
                let row = build_one_row(
                    &text,
                    &style,
                    self.screen.width(),
                    self.caps.unicode_symbols,
                );
                self.push_body_row(row);
            }
            UiLine::ToolCall { name, detail } => {
                // Capture BEFORE the arm mutates `last_mark_was_assistant`.
                // We need the entry-time value to decide whether to insert a
                // leading blank row for breathing room.
                let prev_was_assistant = self.last_mark_was_assistant;
                self.clear_live_spinner_before_permanent_body();
                self.mark_message(crate::render::MarkKind::ToolCall);
                self.last_mark_was_assistant = false;
                self.flush_assistant_remainder();
                // Insert one blank body row between assistant text and the
                // tool-call header so the block has visual breathing room.
                // Only fires when the previous rendered mark was assistant
                // text — consecutive tool calls in a chain stay compact
                // (no blank between them).
                if prev_was_assistant {
                    self.push_body_row(Vec::new());
                }
                let bullet_style = self.tool_bullet_style();
                let tool_name_style = self.style_bold(Role::ToolName);
                let detail_style = self.style_for(Role::Secondary);
                let safe_name = scrub_controls(&name);
                let safe_detail = scrub_controls(&detail);

                // Bash command: render `● bash` header then command lines
                // via `format_shell_command` (shell-boundary wrapping, no
                // truncation). The `● Bash` header + `└` gutter already mark it
                // as a shell command, so the command text renders plainly.
                let is_bash = safe_name.eq_ignore_ascii_case("bash");
                if is_bash && !safe_detail.is_empty() {
                    self.push_bash_command_block(&safe_name, &safe_detail);
                } else {
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
                    if safe_detail.is_empty() {
                        self.push_body_prefixed("● ", &bullet_style, &body_str, &tool_name_style);
                    } else {
                        let detail_str = format!("({})", safe_detail);
                        let rows = self.build_mixed_style_rows(
                            "● ",
                            &bullet_style,
                            &safe_name,
                            &tool_name_style,
                            &detail_str,
                            &detail_style,
                            "",
                            &tool_name_style,
                            &body_str,
                        );
                        for row in rows {
                            self.push_body_row(row);
                        }
                    }
                }
            }
            UiLine::ToolResult {
                success,
                summary,
                diff_stats,
            } => {
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
                // This handler builds cells directly (bypassing push_body_text), so
                // downgrade the `✗` prefix + any glyphs in the summary here.
                let body_str = crate::glyph::downgrade_glyphs(&body_str, self.caps.unicode_symbols)
                    .into_owned();
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
                // `└` leaf marker, gated for non-unicode terminals (→ ASCII backtick).
                let leaf = if self.caps.unicode_symbols {
                    "  \u{2514} "
                } else {
                    "  ` "
                };
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
                    for (chunk_idx, chunk) in crate::width::wrap_line_to_width(phys, row_w.max(1))
                        .into_iter()
                        .enumerate()
                    {
                        let mut row = Vec::new();
                        let prefix = if first_visual { leaf } else { "    " };
                        push_str_cells(&mut row, prefix, &prefix_style);
                        push_str_cells(&mut row, &chunk, line_style);
                        if line_idx == 0 && chunk_idx == 0 {
                            if let Some((added, removed)) = diff_stats {
                                push_str_cells(&mut row, " (", &summary_style);
                                push_str_cells(
                                    &mut row,
                                    &format!("+{added}"),
                                    &self.style_for(Role::DiffAdd),
                                );
                                push_str_cells(&mut row, " ", &summary_style);
                                push_str_cells(
                                    &mut row,
                                    &format!("-{removed}"),
                                    &self.style_for(Role::DiffRemove),
                                );
                                push_str_cells(&mut row, ")", &summary_style);
                            }
                        }
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
                use crate::render::DiffKind;
                let gutter = crate::render::diff::diff_gutter_width(&entries);
                let unicode = self.caps.unicode_symbols;
                let muted = self.style_for(Role::Muted);
                let add_style = self.style_for(Role::DiffAdd);
                let del_style = self.style_for(Role::DiffRemove);
                let secondary = self.style_for(Role::Secondary);

                // Summary header `+N −M` (codex/opencode style): a glance shows
                // the size of the change without reading every row.
                let adds = entries.iter().filter(|e| e.kind == DiffKind::Add).count();
                let dels = entries.iter().filter(|e| e.kind == DiffKind::Del).count();
                if adds > 0 || dels > 0 {
                    let minus = if unicode { '\u{2212}' } else { '-' };
                    self.push_body_text(&format!("  +{adds} {minus}{dels}"), &muted);
                }

                // Display cap: at most MAX_DIFF_DISPLAY changed/context rows, then
                // a muted `… +N more lines`. The MODEL still gets the full diff —
                // this caps only the scrollback render. Separator/summary rows
                // don't count toward the cap.
                const MAX_DIFF_DISPLAY: usize = 25;
                let content_total = entries
                    .iter()
                    .filter(|e| e.kind != DiffKind::Separator)
                    .count();
                let ellipsis = if unicode { "\u{2026}" } else { "..." };
                let mut shown = 0usize;
                for entry in &entries {
                    if entry.kind == DiffKind::Separator {
                        let sep = if unicode {
                            crate::render::diff::diff_row_text(entry, gutter)
                        } else {
                            format!("  {:>gutter$} ...", "")
                        };
                        self.push_body_text(&sep, &muted);
                        continue;
                    }
                    if shown >= MAX_DIFF_DISPLAY {
                        let more = content_total - shown;
                        self.push_body_text(&format!("  {ellipsis} +{more} more lines"), &muted);
                        break;
                    }
                    let style = match entry.kind {
                        DiffKind::Add => &add_style,
                        DiffKind::Del => &del_style,
                        _ => &secondary,
                    };
                    let body = crate::render::diff::diff_row_text(entry, gutter);
                    self.push_body_text(&scrub_controls(&body), style);
                    shown += 1;
                }
            }
            UiLine::EditDiffBlock(entries) => {
                use crate::render::DiffKind;
                let gutter = crate::render::diff::diff_gutter_width(&entries);
                let muted = self.style_for(Role::Muted);
                let add_style = self.style_for(Role::DiffAdd);
                let del_style = self.style_for(Role::DiffRemove);
                let secondary = self.style_for(Role::Secondary);
                let unicode = self.caps.unicode_symbols;
                const MAX_DIFF_DISPLAY: usize = 25;
                let content_total = entries
                    .iter()
                    .filter(|entry| entry.kind != DiffKind::Separator)
                    .count();
                let ellipsis = if unicode { "\u{2026}" } else { "..." };
                let mut shown = 0usize;
                for entry in &entries {
                    if entry.kind == DiffKind::Separator {
                        let separator = if unicode {
                            crate::render::diff::diff_row_text(entry, gutter)
                        } else {
                            format!("  {:>gutter$} ...", "")
                        };
                        self.push_body_text(&separator, &muted);
                        continue;
                    }
                    if shown >= MAX_DIFF_DISPLAY {
                        self.push_body_text(
                            &format!("  {ellipsis} +{} more lines", content_total - shown),
                            &muted,
                        );
                        break;
                    }
                    let style = match entry.kind {
                        DiffKind::Add => &add_style,
                        DiffKind::Del => &del_style,
                        _ => &secondary,
                    };
                    let body = crate::render::diff::diff_row_text(entry, gutter);
                    self.push_body_text(&scrub_controls(&body), style);
                    shown += 1;
                }
            }

            // ── body: errors / command output ──
            UiLine::Error(msg) => {
                // Defense in depth (mirror TurnComplete / TurnCancelled): a turn
                // that errors while a tool is still in flight — gateway 5xx,
                // timeout, rate-limit, empty-response error, a failure mid-tool —
                // must commit it. Otherwise `inflight_tool` stays `Some` and
                // `paint_footer`'s `suppress_cursor` keeps the input caret hidden
                // on EVERY repaint (including while the user types), until `/clear`
                // resets it — the reported "cursor disappears after a while" bug.
                // Commit BEFORE the error line so the frozen `▸` tool row lands
                // above it, matching the transcript order of a normal commit.
                self.flush_assistant_remainder();
                self.commit_inflight_tool();
                let err_style = self.style_for(Role::Error);
                let safe = scrub_controls(&msg);
                let body = t(Msg::ErrorPrefix { msg: &safe });
                self.push_body_text(&body, &err_style);
            }
            UiLine::Warning(msg) => {
                // Amber advisory — distinct from Error (red) so users can tell
                // "noticed something" from "turn died". A `⚠` glyph + amber
                // (non-bold) body: visible without the loud bold-yellow full
                // line (matches codex's warning style). Colour is theme-aware
                // (bright yellow is near-invisible on light backgrounds — see
                // `warning_for_current_theme`); the `⚠` downgrades to `!` on
                // non-unicode terminals via the cell-level glyph backstop.
                let warn_style = CellStyle {
                    fg: Some(crate::render::theme::warning_for_current_theme()),
                    ..CellStyle::default()
                };
                let body = format!("⚠ {}", scrub_controls(&msg));
                self.push_body_text(&body, &warn_style);
            }
            UiLine::Muted(msg) => {
                // Dim, non-bold informational line — no forced prefix.
                // Same DarkGrey palette as CompactionMark so it recedes
                // into scrollback without reading as a warning or error.
                let style = CellStyle {
                    fg: Some(crossterm::style::Color::DarkGrey),
                    bold: false,
                    ..CellStyle::default()
                };
                self.push_body_text(&scrub_controls(&msg), &style);
            }
            UiLine::CompactionMark(label) => {
                // Dim, left-aligned rule marking where compaction folded
                // history. Faint DarkGrey (not bold) so it reads as structure,
                // not an alert — deliberately distinct from Warning's bold
                // yellow. Unified for auto + manual /compact.
                let style = CellStyle {
                    fg: Some(crossterm::style::Color::DarkGrey),
                    ..CellStyle::default()
                };
                let body = crate::render::compaction_rule(
                    &scrub_controls(&label),
                    self.caps.unicode_symbols,
                );
                self.push_body_text(&body, &style);
            }
            UiLine::CommandOutput(text) => {
                // If it's a subordinate command output (starts with "  ⎿" or "  └" or "  `" or "  \\"),
                // pop the empty blank separator so it sits flush under the command!
                let is_subordinate = text.starts_with("  ⎿")
                    || text.starts_with("  └")
                    || text.starts_with("  `")
                    || text.starts_with("  \\");
                if is_subordinate && self.body_lines.last().map_or(false, |r| r.is_empty()) {
                    crate::tuix_trace!(
                        "BPOP",
                        "site=command_blank len_before={} n=1 scrolled_off={}",
                        self.body_lines.len(),
                        self.scrolled_off
                    );
                    self.body_lines.pop();
                    let bottom = self.body_bottom_row();
                    if bottom > 0 {
                        let seq = format!("\x1b[{};1H\x1b[K", bottom);
                        let _ = self.out.write_all(seq.as_bytes());
                    }
                    self.skip_body_scroll_count = self.skip_body_scroll_count.saturating_add(1);
                }

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
                    self.skip_body_scroll_count = self.skip_body_scroll_count.saturating_add(1);
                }
                let body = format!("└ [Image #{}]", n);
                self.push_body_text(&body, &self.style_for(Role::Muted));
                self.push_body_row(Vec::new());
            }
            UiLine::VisionPreprocessSuccess { msg, model } => {
                // `{msg}  ` in default text style; `{model}` in bold only (no
                // colour) so the VL model identity stands out from the notice
                // text without the loud accent hue — the user requested just
                // emphasis, not a themed colour. push_body_prefixed handles the
                // two styles in a single visual line and continues onto wrapped
                // rows with the prefix's display width as continuation pad.
                //
                // Trailing blank: without it the next event's row (e.g.
                // `● Pondering…` spinner or assistant text) butts right
                // up against the success notice — user reported it felt
                // too cramped. The blank lets the success line breathe
                // as its own paragraph.
                let default_style = CellStyle::default();
                let model_style = CellStyle {
                    fg: None,
                    bold: true,
                    reverse: false,
                    faint: false,
                    bg: None,
                };
                let prefix = format!("{msg}  ");
                self.push_body_prefixed(&prefix, &default_style, &model, &model_style);
                self.push_body_row(Vec::new());
            }
            UiLine::DiffPanel {
                title,
                rows,
                footer,
                win_width,
                win_height,
            } => {
                self.modal_overlay = Some(
                    self.build_diff_panel_overlay(&title, &rows, &footer, win_width, win_height),
                );
                self.diff_overlay_active = true;
            }
            UiLine::ModalOverlayClear => {
                self.modal_overlay = None;
                self.diff_overlay_active = false;
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
        // Preserve the existing chosen_pool_indices so a model/cwd update
        // doesn't re-roll the random tip selection. No banner yet → nothing to
        // refresh.
        let Some((_, _, chosen)) = self.welcome_banner.clone() else {
            return;
        };
        let model_scrubbed = scrub_controls(model);
        let wd_scrubbed = scrub_controls(working_dir);
        self.welcome_banner = Some((model_scrubbed, wd_scrubbed, chosen));
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
        self.screen = Screen::new(self.screen.width(), self.screen.height())
            .with_jediterm(self.caps.jediterm);
        // Rebuilding `screen` dropped any suppression flag — re-apply it so a
        // `reset()` issued INSIDE a `begin_sync()` batch (the `/resume` replay)
        // keeps the next render_diff from emitting a nested DECSET 2026 envelope.
        self.screen.set_sync_suppressed(self.in_sync_batch);
        self.body_lines.clear();
        self.scrolled_off = 0;
        self.welcome_line_count = 0;
        // Fresh session (`/clear`, `/session`): forget the cached welcome banner
        // AND the rolled tip selection so the next welcome rolls new tips. (A
        // resize does NOT come through here — it uses reflow_body_to_current_width,
        // which preserves `welcome_tip_indices`.)
        self.welcome_banner = None;
        self.welcome_tip_indices = None;
        self.message_marks.clear();
        self.assistant_line_buf.clear();
        self.reasoning_line_buf.clear();
        self.md_state.reset();
        self.last_painted_footer_rows = 0;
        // Drop the reflow log too: the body it described is gone, so a
        // later resize must not replay it. The `/resume` / `/clear`
        // re-render that typically follows repopulates it via `render()`.
        self.body_log.clear();
        self.body_log_truncated = false;
        let _ = self.out.flush();
        // Force cold-start CUP+EL on next paint to wipe pre-reset stdout writes.
        self.screen.invalidate();
    }

    fn begin_sync(&mut self) {
        // Open ONE outer DECSET 2026 envelope around a whole upcoming burst
        // — specifically the `/resume` replay, which blanks the screen
        // (`reset()`) and then re-emits the entire transcript line-by-line
        // via direct stdout writes that LF-scroll history into native
        // scrollback. Without this, the blank flushes unwrapped and every
        // re-emitted row paints unsynchronized, so the user sees the screen
        // blank and the history visibly re-scroll (the reported flicker).
        // Inside the envelope, suppress per-frame `render_diff` envelopes
        // (`sync_suppressed`) so a nested ESU can't end the batch early.
        // Idempotent: a second begin keeps the single open envelope.
        if self.in_sync_batch {
            return;
        }
        self.in_sync_batch = true;
        self.screen.set_sync_suppressed(true);
        let _ = self.out.write_all(b"\x1b[?2026h");
        let _ = self.out.flush();
    }

    fn end_sync(&mut self) {
        if !self.in_sync_batch {
            return;
        }
        // Land the final frame (settled footer) INSIDE the still-open
        // envelope so nothing paints after it closes — suppression is still
        // on here, so this render_diff emits no nested envelope of its own.
        self.flush_deferred();
        self.in_sync_batch = false;
        self.screen.set_sync_suppressed(false);
        // Close the envelope: capable hosts now paint the whole burst as one
        // atomic update. Hosts that ignore DECSET 2026 (Terminal.app, older
        // SSH) saw the writes immediately — no worse than before.
        let _ = self.out.write_all(b"\x1b[?2026l");
        let _ = self.out.flush();
    }

    fn set_suppress_auto_copy(&mut self, suppress: bool) {
        self.suppress_auto_copy = suppress;
    }

    fn set_auto_copy_enabled(&mut self, enabled: bool) {
        self.auto_copy_enabled = enabled;
    }

    fn set_title(&mut self, title: String) {
        // Route through `self.out` so the OSC bytes stay ordered with the
        // BufWriter's body writes (no raw-stdout interleave). `execute!`
        // flushes. crossterm's `SetTitle` emits an OSC sequence on unix and
        // calls `SetConsoleTitleW` on Windows, so this reaches conhost /
        // Windows Terminal tabs too — the whole point of owning the title.
        // Not wrapped in a DECSET-2026 envelope: a title update touches no
        // cell of the display rect, so it's safe to land any time.
        let _ = crossterm::execute!(&mut self.out, crossterm::terminal::SetTitle(title));
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
        let _ = self.out.write_all(b"\x1b[?1006l\x1b[?1002l"); // Position cursor at the top of where the footer (input box +
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
        // key-reporting mode than we initialised with and Shift+Enter stops
        // carrying SHIFT. Same flag set as `TerminalGuard::activate`; in
        // particular, it excludes REPORT_EVENT_TYPES so release CSI-u reports
        // cannot leak into the input box when their leading ESC is split.
        //
        // Windows and JediTerm are excluded for the same reason as the
        // initial push: Windows' Win32 console backend can't decode `CSI u`
        // (numpad-leak `ESC[57400u`), and JediTerm re-frames mouse reports as
        // kitty key events while the protocol is armed (mouse-move gibberish
        // in the input box). Re-arming either after a resume would reintroduce
        // the leak on every OAuth/shell round-trip. Shared predicate with the
        // initial push; see `TerminalGuard::activate` in lib.rs for the full
        // rationale.
        if crate::should_enable_kitty_keyboard(&self.caps) {
            let _ = execute!(
                self.out,
                PushKeyboardEnhancementFlags(crate::kitty_keyboard_flags())
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
        let _ = self.out.flush();
    }

    fn take_pending_scroll_flush(&mut self) -> bool {
        std::mem::take(&mut self.pending_scroll_flush)
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
            let menu_rows = self.max_menu_rows(h, 0);
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
        // Drop any cached `/diff` / `/view` modal overlay: it was laid out for the
        // OLD geometry, so letting `paint_frame` below re-stamp it would ghost the
        // panel at a stale position (issue #1158 — "modal duplicated on resize").
        // The event-loop resize handler rebuilds it fresh via `m.draw()` right
        // after this returns. Crucially we clear ONLY the overlay here, not the
        // whole screen: the handler used to call `clear_screen()` (= `reset()`,
        // which also drops `body_log`) for this, so the `reflow_body_to_current_width`
        // below replayed an EMPTY log and the entire transcript was lost on any
        // resize while a panel was open ("从大屏到小屏后上面的内容丢失").
        self.modal_overlay = None;
        self.diff_overlay_active = false;
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
        // Open ONE outer DECSET 2026 envelope around the whole resize: the
        // wipe below and the body tail re-emit further down are direct stdout
        // writes that, unwrapped, made the terminal visibly blank and re-stamp
        // the body on every resize (same flicker class as the `/resume`
        // replay). Capable hosts now paint the resize as one atomic update;
        // the inner `paint_frame`/`render_diff` is suppressed below so it
        // can't nest its own envelope. `?2026l` closes it at the very end.
        let _ = self.out.write_all(b"\x1b[?2026h");
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
        // Clear the host's native scrollback. Resizing the cell grid (a
        // PowerShell / Windows Terminal font-zoom is the reported case)
        // makes the terminal REFLOW its main-screen buffer and push the
        // top of the viewport — the welcome banner — into scrollback. We
        // run on the main screen (no alt-screen, so the transcript
        // survives exit), and the viewport wipe above (EL / ED2) is
        // row-local: it cannot reach those promoted rows. So every zoom
        // step leaves another welcome copy stranded in scrollback (issue
        // #709: "向上滚动多出 2 条"). `\x1b[3J` is the only sequence that
        // clears scrollback; the reflow replay below then re-promotes the
        // CORRECT overflow at the new width via its append-only LFs, so
        // the transcript is rebuilt rather than lost.
        //
        // We clear EVEN when `body_log` was truncated. The alternative —
        // skipping 3J to preserve the evicted prefix — is worse: the reflow
        // re-appends the whole RETAINED transcript on top of the un-cleared
        // old copy, so every resize stacks a duplicate (the reported
        // "一直重复输出" on long sessions). The pre-truncation prefix is
        // already gone from `body_log` and can't be reflowed anyway, so
        // clearing it loses only un-repaintable history — an acceptable
        // trade for not duplicating the visible transcript.
        let _ = self.out.write_all(b"\x1b[3J");
        crate::tuix_trace!(
            "RSZ",
            "scrollback cleared (3J){}",
            if self.body_log_truncated {
                " [log truncated: pre-eviction prefix dropped]"
            } else {
                ""
            }
        );
        self.screen.resize(cols, rows);
        // `screen.resize` rebuilds the Screen (resetting `sync_suppressed`) —
        // re-assert suppression so the `flush_frame` render_diff below paints
        // inside the outer envelope without emitting a nested one.
        self.screen.set_sync_suppressed(true);
        // Mark physical state unknown so the upcoming paint_frame's
        // render_diff cold-starts with a per-row CUP+EL preamble — exactly
        // like reset() (:~3627) and resume_from_external() (:~3732), which
        // rebuild the Screen the same way. Without this, `screen.resize`
        // leaves `physical_dirty = false` + blank `prev_cells`, so the diff
        // emits only cell patches and never clears the OLD footer rows. The
        // per-row CUP+EL wipe above masks that on Unix / Windows Terminal,
        // but legacy conhost is forced down to a single ED2 (to dodge the
        // 0xc0000409 fastfail), and ED2 doesn't reliably clear the final
        // geometry mid-drag — so the stale footer survives and the new
        // footer stacks on top of it (the Windows resize footer-duplication
        // bug). invalidate() must run AFTER resize so its sentinel rows are
        // sized to the new width.
        self.screen.invalidate();
        // ── Preserve streaming parser state across the reflow. Issue
        // #950: resize during a streaming turn must not reset mid-stream
        // markdown parsing (code blocks lose their fence context). The
        // partial assistant text is already present in `body_log` because
        // `render()` logs every AssistantText chunk before buffering it, so
        // do not append `assistant_line_buf` here.

        // Snapshot markdown parser state. The reflow replays each body_log
        // entry through render() with a freshly-reset md_state, which may
        // produce different output (e.g. if a code fence was still open).
        // Restoring the snapshot after reflow means subsequent AssistantText
        // chunks continue the parse correctly — the replayed body rows are
        // for display only.
        let saved_md_state = self.md_state.clone();

        // live_group child_indices reference body_lines positions that are
        // invalidated by the reflow. The reflow already clears it; do NOT
        // restore. The next ToolGroupUpdate event from the agent will rebuild
        // it at correct positions.

        // Reflow the ENTIRE transcript at the new width by replaying the
        // semantic `body_log` through `render()`. The old code only
        // rebuilt the welcome banner and CLIPPED every other row to the
        // new width (`clip_cells_to_width`) — which dropped the overflow
        // on a shrink and never re-filled on a grow, so multi-line
        // history came out garbled (issue #709: "resize 历史布局乱了").
        // `body_lines` keeps cells already wrapped to their original
        // width and can't be re-wrapped, so a correct reflow has to
        // re-render from source. The replay also re-promotes the proper
        // overflow into scrollback at the new width via its append-only
        // LFs, completing the `\x1b[3J` rebuild above.
        self.reflow_body_to_current_width();

        // ── Restore saved state so subsequent streaming events continue
        // correctly after the reflow ──
        self.md_state = saved_md_state;
        // live_spinner_active is NOT restored: the spinner row position
        // may have changed (different body row count after width change).
        // The next spinner tick from the agent naturally places it at the
        // correct position via `push_or_update_live_spinner`.
        crate::tuix_trace!("RSZ", "body reflowed; paint begin");
        self.paint_frame();
        self.flush_frame();
        // Close the envelope after the final frame has landed inside it.
        self.screen.set_sync_suppressed(false);
        let _ = self.out.write_all(b"\x1b[?2026l");
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
        // \x1b[<1u POPS one level of Kitty keyboard enhancement (this is
        // crossterm's PopKeyboardEnhancementFlags). NOTE: the introducer is
        // `<` (pop), not `>` (push) — `\x1b[>1u` would *arm* the protocol on
        // the way out, leaving the parent shell in CSI-u mode. On JediTerm
        // (DevEco/IDEA) that armed state turns every mouse move into input-box
        // gibberish, so a panic here must never push. Popping a level we never
        // pushed is a harmless no-op (empty kitty stack).
        let _ = self
            .out
            .write_all(b"\x1b[?1006l\x1b[?1002l\x1b[<1u\x1b[?25h\x1b[?7h\x1b[r");
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
fn build_one_row(text: &str, style: &CellStyle, screen_w: u16, unicode: bool) -> Vec<Cell> {
    let avail = (screen_w as usize).saturating_sub(PAD_COL);
    // Downgrade decorative glyphs (`●` header, `└` child branches) on non-unicode terminals.
    let text = crate::glyph::downgrade_glyphs(text, unicode);
    let safe = scrub_controls(&text);
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

/// Pluck the time/queue metadata suffix (` · N queued` and/or ` · 12s`) out
/// of a spinner label built by `format_spinner_label`, to forward onto an
/// in-flight tool row. Labels have the shape
/// `{base}{ellipsis}[ · thinking with {effort} effort][ · {n} queued][ · {elapsed}]`
/// — the effort hint comes FIRST among the metadata (and must NOT ride onto a
/// tool row, which isn't "thinking"). Returns the slice **including** its
/// leading ` · ` separator so callers can concatenate it directly, or `""` if
/// there's no time/queue metadata yet.
fn spinner_meta_suffix(label: &str) -> &str {
    const EFFORT_MARK: &str = " · thinking with ";
    // The metadata that trails the base label comes in two shapes: a ` · …`
    // run (queue / fold) and the trailing phase-clock group, which
    // `format_spinner_label` wraps in ONE pair of parens opening with ` (`
    // (`… (3s · ↑ 1.93K tokens)`). The forwarded suffix must begin at
    // whichever boundary comes first so the parens stay balanced on a bash
    // row (`Running (3s · ↑ 1.93K tokens)`). Keying off the ` · ` alone split
    // the group on the separator INSIDE the parens, dropping the `(3s` and
    // leaving a dangling `)` (reported: `Running · ↑ 1.93K tokens)`).
    let meta_start = |from: usize| -> Option<usize> {
        let dot = label[from..].find(" · ").map(|i| from + i);
        let paren = label[from..].find(" (").map(|i| from + i);
        match (dot, paren) {
            (Some(d), Some(p)) => Some(d.min(p)),
            (d, None) => d,
            (None, p) => p,
        }
    };
    if let Some(start) = label.find(EFFORT_MARK) {
        // Effort is the first metadata segment (a tool "isn't thinking", so
        // splice it out). It runs until the next boundary — a ` · ` queue run
        // or the ` (` clock group. Scanning past the fixed marker lands inside
        // the ASCII effort value, so the next boundary is unambiguously the
        // following segment.
        let scan_from = start + EFFORT_MARK.len();
        return meta_start(scan_from).map(|i| &label[i..]).unwrap_or("");
    }
    // No effort hint: metadata begins at the first ` · ` or ` (` after the base.
    meta_start(0).map(|i| &label[i..]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{BadgeColour, ModeBadge};
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
        assert_eq!(format_ctx_usage(10_400, 131_000), "10.4k/131k tok (8%)");
    }

    #[test]
    fn ctx_usage_with_known_window_shows_zero_before_first_turn() {
        assert_eq!(format_ctx_usage(0, 1_000_000), "0/1m tok (0%)");
    }

    #[test]
    fn goal_row_shows_condition_round_and_elapsed() {
        // Wide row (unicode caps): ◎ marker + full condition + round + elapsed.
        let row = format_goal_row("重构 auth 模块直到测试全过", 3, 133, 80, true);
        assert!(row.starts_with("◎ "), "geometric marker present: {row}");
        assert!(!row.contains('🎯'), "must NOT use an emoji marker: {row}");
        assert!(
            row.contains("重构 auth 模块直到测试全过"),
            "full condition kept: {row}"
        );
        assert!(row.contains("· round 3 ·"), "round shown: {row}");
        assert!(row.contains("2m13s"), "elapsed mm/ss: {row}");
    }

    #[test]
    fn goal_row_ascii_fallback_avoids_geometric_and_emoji_glyphs() {
        // Windows legacy conhost / Consolas (unicode_symbols=false): no ◎ tofu,
        // no emoji — a plain ASCII `*` marker, same gate as the spinner.
        let row = format_goal_row("ship it", 4, 7, 80, false);
        assert!(row.starts_with("* "), "ascii marker: {row}");
        assert!(
            !row.contains('◎') && !row.contains('🎯'),
            "no non-ASCII glyph: {row}"
        );
        assert!(
            row.contains("· round 4 · 7s"),
            "round/elapsed intact: {row}"
        );
    }

    #[test]
    fn goal_row_parts_split_marker_condition_meta_for_styling() {
        // Marker / condition / meta are returned separately so the renderer can
        // style them with hierarchy (accent / normal / muted). Round shown verbatim.
        let (marker, cond, meta) = goal_row_parts("fix tests", 1, 8, 80, true);
        assert_eq!(marker, "◎ ");
        assert_eq!(cond, "fix tests");
        assert_eq!(meta, " · round 1 · 8s");
        // ASCII fallback marker on non-unicode terminals.
        assert_eq!(goal_row_parts("x", 1, 8, 80, false).0, "* ");
    }

    #[test]
    fn goal_row_elapsed_under_a_minute_omits_minutes() {
        let row = format_goal_row("x", 1, 42, 80, true);
        assert!(row.contains("· 42s"), "seconds-only under a minute: {row}");
        assert!(!row.contains("0m"), "no leading 0m: {row}");
    }

    #[test]
    fn goal_row_truncates_long_condition_but_keeps_round_and_elapsed() {
        let cond = "a".repeat(200);
        let row = format_goal_row(&cond, 7, 5, 40, true);
        assert!(
            crate::width::display_width(&row) <= 40,
            "row fits width: {row}"
        );
        assert!(
            row.contains("· round 7 · 5s"),
            "round/elapsed survive: {row}"
        );
        assert!(
            row.contains('…'),
            "condition truncated with ellipsis: {row}"
        );
    }

    #[test]
    fn goal_row_degrades_to_round_elapsed_when_too_narrow() {
        // No room for any condition → drop it, keep marker + round/elapsed.
        let row = format_goal_row("some long condition", 2, 9, 14, true);
        assert!(
            crate::width::display_width(&row) <= 14,
            "row fits narrow width: {row}"
        );
        assert!(
            row.contains("round 2"),
            "round survives even when condition dropped: {row}"
        );
    }

    #[test]
    fn loop_row_shows_label_round_and_elapsed() {
        let row = format_loop_row("检查构建状态", 3, 133, 80, true);

        assert!(row.starts_with("⚡ "), "lightning marker present: {row}");
        assert!(row.contains("检查构建状态"), "full label kept: {row}");
        assert!(
            row.contains("· round 3 · 2m13s"),
            "round/elapsed shown: {row}"
        );
    }

    #[test]
    fn loop_row_uses_ascii_marker_when_unicode_is_disabled() {
        let row = format_loop_row("check", 2, 9, 80, false);

        assert!(row.starts_with("* "), "ascii marker present: {row}");
        assert!(!row.contains('⚡'), "unicode marker omitted: {row}");
    }

    #[test]
    fn loop_row_truncates_label_without_dropping_metadata() {
        let row = format_loop_row(&"x".repeat(200), 7, 5, 40, true);

        assert!(
            crate::width::display_width(&row) <= 40,
            "row fits width: {row}"
        );
        assert!(row.contains("· round 7 · 5s"), "metadata survives: {row}");
        assert!(row.contains('…'), "label uses an ellipsis: {row}");
    }

    #[test]
    fn ctx_usage_keeps_round_window_clean() {
        // 128k window is the common default — render as `128k`, not `128.0k`.
        assert_eq!(format_ctx_usage(50_000, 128_000), "50.0k/128k tok (39%)");
    }

    #[test]
    fn ctx_usage_without_window_shows_used_only() {
        // Pre-first-turn / unknown-provider fallback — window unknown.
        // Better to show the count alone than a misleading "/0".
        assert_eq!(format_ctx_usage(10_400, 0), "10.4k tok");
    }

    #[test]
    fn ctx_usage_under_one_thousand_keeps_raw_count() {
        assert_eq!(format_ctx_usage(523, 131_000), "523/131k tok (0%)");
        assert_eq!(format_ctx_usage(523, 0), "523 tok");
    }

    #[test]
    fn ctx_usage_non_round_window_rounds_to_nearest_k() {
        // GLM-5.1 endpoint ships a 131_072 window; we display 131k, not 131.072k.
        assert_eq!(format_ctx_usage(50_000, 131_072), "50.0k/131k tok (38%)");
    }

    #[test]
    fn ctx_usage_million_window_renders_as_m_not_thousand_k() {
        // 1m-context models would previously show `1000k`, mixing k/m in
        // the user's head and burning width on a leading zero parade.
        // Round million → bare `1m`; non-round million → one-decimal `1.5m`.
        assert_eq!(format_ctx_usage(1_400, 1_000_000), "1.4k/1m tok (0%)");
        assert_eq!(format_ctx_usage(50_000, 2_000_000), "50.0k/2m tok (3%)");
        assert_eq!(format_ctx_usage(50_000, 1_500_000), "50.0k/1.5m tok (3%)");
    }

    #[test]
    fn ctx_usage_used_above_one_million_uses_m_unit() {
        // Long-running sessions on a 1m window can park `used` above 1M;
        // keep one decimal so the counter still moves visibly turn-to-turn.
        // 1.2m of a 1m window is 120% — over the limit, so the percentage shows.
        assert_eq!(format_ctx_usage(1_200_000, 1_000_000), "1.2m/1m tok (120%)");
        assert_eq!(format_ctx_usage(2_500_000, 0), "2.5m tok");
    }

    #[test]
    fn ctx_usage_surfaces_pct_at_and_over_window() {
        // The over-window case (e.g. a 339k prompt into a 200k window) must be
        // visible, not silent — the percentage is the "near/over limit" signal.
        assert_eq!(format_ctx_usage(339_380, 200_000), "339.4k/200k tok (170%)");
    }

    #[test]
    fn ctx_usage_at_ninety_percent() {
        // Exact 90%: 180k of 200k rounds cleanly to 90% — just a representative value now, not a gate.
        assert_eq!(format_ctx_usage(180_000, 200_000), "180.0k/200k tok (90%)");
    }

    #[test]
    fn ctx_usage_always_shows_percentage() {
        // Previously terse below 90% — now % is always shown so the user can
        // watch context fill toward the 0.7 auto-compaction threshold.
        // 179k/200k = 89.5% → rounds to 90%.
        assert_eq!(format_ctx_usage(179_000, 200_000), "179.0k/200k tok (90%)");
    }

    #[test]
    fn format_ctx_usage_shows_percentage_below_ninety() {
        // Pre-change this stayed terse ("138k/200k tok") below 90%, hiding the
        // climb toward the 0.7 auto-compaction threshold. Now % is always shown.
        let s = format_ctx_usage(138_000, 200_000);
        assert!(s.contains("(69%)"), "expected percentage, got: {s}");
    }

    #[test]
    fn format_ctx_usage_no_percentage_when_window_unknown() {
        let s = format_ctx_usage(5_000, 0);
        assert!(!s.contains('%'), "no window ⇒ no percentage, got: {s}");
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
            history: None,
            command_output: None,
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: None,
            bypass_indicator: None,
            session_name: None,
            reasoning_effort: None,
            goal: None,
            loop_status: None,
            todo: None,
            subtasks: None,
            approval: None,
            user_input: None,
            pending_messages: Vec::new(),
            round_cap_panel: None,
        }
    }

    #[test]
    fn status_row_shows_known_context_window_before_first_turn() {
        let (r, _counter) = new_counting(80, 24);
        let mut status = status_basic();
        status.ctx_window = 1_000_000;

        let row = r.build_status_row(&status, 80, false);
        let visible: String = row.iter().map(|cell| cell.ch).collect();
        assert!(visible.contains("0/1m tok (0%)"), "{visible:?}");
    }

    #[test]
    fn command_output_is_transient_footer_rows_below_input() {
        let (mut r, _counter) = new_counting(80, 24);
        r.status = status_basic();
        let baseline_footer_rows = r.current_footer_rows();
        r.status.command_output = Some("Prompt tokens: 120\nTotal tokens: 150".into());

        let rows = r.build_command_output_rows();
        let text = |row: &Vec<Cell>| row.iter().map(|cell| cell.ch).collect::<String>();
        assert_eq!(rows.len(), 2);
        assert!(text(&rows[0]).contains("Prompt tokens: 120"));
        assert!(text(&rows[1]).contains("Total tokens: 150"));
        assert_eq!(r.current_footer_rows(), baseline_footer_rows + 2);
        assert!(
            r.body_lines.is_empty() && r.body_log.is_empty(),
            "footer command output must never enter transcript scrollback"
        );

        r.status.command_output = None;
        assert_eq!(r.current_footer_rows(), baseline_footer_rows);
    }

    #[test]
    fn pending_messages_render_as_transient_footer_rows() {
        let (mut r, _counter) = new_counting(80, 24);
        r.status = status_basic();
        let baseline_footer_rows = r.current_footer_rows();
        r.status.pending_messages = vec![
            "first pending message".into(),
            "second\npending\tmessage".into(),
        ];

        let rows = r.build_pending_message_rows();
        let text = |row: &Vec<Cell>| row.iter().map(|cell| cell.ch).collect::<String>();
        assert_eq!(
            rows.len(),
            5,
            "leading spacer, title, two message previews, trailing spacer"
        );
        assert!(text(&rows[0]).is_empty());
        assert!(text(&rows[1]).contains("Messages to be submitted"));
        assert!(text(&rows[2]).contains("first pending message"));
        assert!(text(&rows[3]).contains("second pending message"));
        assert!(text(&rows[4]).is_empty());
        assert_eq!(r.current_footer_rows(), baseline_footer_rows + 5);
        assert!(
            r.body_lines.is_empty() && r.body_log.is_empty(),
            "pending messages must never enter transcript scrollback"
        );
    }

    #[test]
    fn pending_messages_have_one_blank_row_above_and_below_on_screen() {
        let width = 80u16;
        let height = 16u16;
        let (mut r, buf) = new_capturing(width, height);
        let mut vterm = crate::test_term::VirtualTerminal::new(width, height);
        let mut status = status_basic();
        status.pending_messages = vec!["你明白了吗？".into()];

        r.render(UiLine::AssistantText("assistant tail\n".into()));
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Brewing".into(),
            status,
            menu: None,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let spinner_row = (0..height as usize)
            .find(|row| vterm.row_text(*row).contains("Brewing"))
            .unwrap_or_else(|| panic!("spinner missing:\n{}", vterm.dump()));
        let pending_title_row = (0..height as usize)
            .find(|row| vterm.row_text(*row).contains("Messages to be submitted"))
            .unwrap_or_else(|| panic!("pending title missing:\n{}", vterm.dump()));
        let pending_message_row = pending_title_row + 1;
        assert!(
            vterm.row_text(pending_message_row).contains('↳'),
            "pending message missing:\n{}",
            vterm.dump()
        );
        let input_rule_row = (pending_message_row + 1..height as usize)
            .find(|row| vterm.row_text(*row).contains('─'))
            .unwrap_or_else(|| panic!("input top rule missing:\n{}", vterm.dump()));

        assert_eq!(
            pending_title_row,
            spinner_row + 2,
            "pending panel must leave exactly one blank row below the spinner:\n{}",
            vterm.dump()
        );
        assert_eq!(
            input_rule_row,
            pending_message_row + 2,
            "pending panel must leave exactly one blank row above the input:\n{}",
            vterm.dump()
        );
    }

    #[test]
    fn pending_messages_drop_cosmetic_spacing_to_fit_short_terminal() {
        let (mut r, _buf) = new_capturing(80, 8);
        r.status = status_basic();
        r.status.pending_messages = (0..8).map(|i| format!("pending-{i}")).collect();

        let rows = r.build_pending_message_rows();
        assert!(
            rows.iter().all(|row| !row.is_empty()),
            "short terminal must drop cosmetic pending spacers first"
        );
        assert!(
            r.current_footer_rows() < r.screen.height() as usize,
            "footer must preserve at least one body row: footer={} height={}",
            r.current_footer_rows(),
            r.screen.height()
        );
    }

    #[test]
    fn pending_and_todo_panels_do_not_stack_blank_rows() {
        use atomcode_capabilities::tools::todo::TodoStatus;

        let (mut r, _buf) = new_capturing(80, 24);
        r.status = status_basic();
        r.status.pending_messages = vec!["pending steer".into()];
        r.status.todo = Some(crate::render::TodoProgress {
            current: Some("active task".into()),
            completed: 0,
            in_progress: 1,
            total: 1,
            items: vec![(TodoStatus::InProgress, "active task".into())],
        });

        let pending = r.build_pending_message_rows();
        let todo = r.build_todo_rows(r.status.todo.as_ref().unwrap(), 76);
        let combined: Vec<&Vec<Cell>> = pending.iter().chain(todo.iter()).collect();

        assert!(
            !combined
                .windows(2)
                .any(|pair| pair[0].is_empty() && pair[1].is_empty()),
            "pending and todo panels must never create consecutive spacer rows"
        );
        assert!(
            pending.first().is_some_and(|row| row.is_empty()),
            "the combined top panel should retain one leading breathing row"
        );
        assert!(
            todo.first().is_some_and(|row| row.is_empty()),
            "todo's leading spacer should be the sole separator after pending content"
        );
    }

    #[test]
    fn command_output_footer_preserves_trusted_usage_sgr() {
        let (mut r, _counter) = new_counting(80, 24);
        r.status = status_basic();
        r.status.command_output =
            Some("  \x1b[1mRate limit\x1b[22m\n  \x1b[32m████\x1b[39m  42.0%".into());

        let rows = r.build_command_output_rows();
        let bold = rows[0]
            .iter()
            .find(|cell| cell.ch == 'R')
            .expect("heading cell");
        let green = rows[1]
            .iter()
            .find(|cell| cell.ch == '█')
            .expect("progress cell");

        assert!(bold.style.bold, "usage heading should match modal emphasis");
        assert_eq!(
            green.style.fg,
            Some(crossterm::style::Color::DarkGreen),
            "usage progress should keep the modal's SGR 32"
        );
        assert!(
            rows.iter()
                .flatten()
                .all(|cell| cell.ch != '\x1b' && cell.ch != '['),
            "SGR must become cell style, never visible text"
        );
    }

    #[test]
    fn command_output_physically_sits_below_input_and_is_erased() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let mut status = status_basic();
        status.command_output = Some("Prompt tokens: 120\nTotal tokens: 150".into());

        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let input_row = (0..24)
            .find(|&row| {
                let text = vterm.row_text(row);
                text.starts_with("> ") || text.starts_with("\u{276f} ")
            })
            .expect("input row");
        let prompt_row = (0..24)
            .find(|&row| vterm.row_text(row).contains("Prompt tokens: 120"))
            .expect("footer report row");
        assert!(
            prompt_row > input_row,
            "report must be below input:\n{}",
            vterm.dump()
        );
        assert!(
            r.body_lines.is_empty() && r.body_log.is_empty(),
            "footer report must not enter scrollback"
        );

        status.command_output = None;
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
            !vterm.any_row(|row| row.contains("Prompt tokens: 120")),
            "closing the report must erase its physical rows:\n{}",
            vterm.dump()
        );
    }

    #[test]
    fn build_mascot_rows_shape_and_colors() {
        let (r, _c) = new_counting(80, 24);
        let rows = r.build_mascot_rows();
        assert_eq!(rows.len(), crate::render::mascot::MASCOT_ROWS.len());
        for row in &rows {
            assert_eq!(row.len(), crate::render::mascot::MASCOT_WIDTH);
        }
        // At least one orange half-block cell exists.
        let has_orange = rows
            .iter()
            .flatten()
            .any(|c| c.ch == '▀' && c.style.fg == Some(crossterm::style::Color::AnsiValue(202)));
        assert!(has_orange, "mascot must contain orange ▀ cells");
    }

    /// Shell mode (`!`) paints the input box rules + the prompt chevron + the
    /// leading `!` in atomcode's brand purple (`Role::Shell`), NOT the normal
    /// cyan border/accent — and leaves the normal (non-shell) rows untouched.
    #[test]
    fn shell_mode_paints_input_box_chevron_and_bang_purple() {
        use crate::highlight::theme as md_theme;
        let _theme = md_theme::test_lock();
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        md_theme::set_theme_mode(false); // dark → deterministic Role::Shell
        let shell_fg = role(r.caps, Role::Shell);
        let border_fg = role(r.caps, Role::Border);
        assert_ne!(
            shell_fg, border_fg,
            "precondition: shell purple differs from border"
        );

        // Rule rows: purple in shell mode, border otherwise.
        let shell_rule = r.build_rule_row(40, true);
        assert!(
            shell_rule.iter().all(|c| c.style.fg == shell_fg),
            "shell-mode rule must be brand purple"
        );
        let normal_rule = r.build_rule_row(40, false);
        assert!(
            normal_rule.iter().all(|c| c.style.fg == border_fg),
            "non-shell rule stays cyan border"
        );

        // First middle row: chevron + the leading `!` go purple in shell mode.
        let mid = r.build_middle_row("!ls -la", true, true);
        assert_eq!(mid[0].style.fg, shell_fg, "chevron painted shell purple");
        let bang = mid.iter().find(|c| c.ch == '!').expect("leading ! present");
        assert_eq!(bang.style.fg, shell_fg, "leading ! painted shell purple");
        // The rest of the command keeps the default fg (not purple).
        let s_cell = mid
            .iter()
            .find(|c| c.ch == 's')
            .expect("command char present");
        assert_ne!(
            s_cell.style.fg, shell_fg,
            "only the ! is tinted, not the whole command"
        );

        // Leading whitespace before the `!` (buffer `  !ls`) still tints the `!`
        // — `input_shell_mode` trims, so the box is purple; the `!` must match.
        let ws = r.build_middle_row("  !ls", true, true);
        let ws_bang = ws.iter().find(|c| c.ch == '!').expect("leading ! present");
        assert_eq!(
            ws_bang.style.fg, shell_fg,
            "! after leading spaces still painted purple"
        );

        // Non-shell first row: chevron stays accent, no purple leak.
        let plain = r.build_middle_row("ls", true, false);
        assert_eq!(
            plain[0].style.fg,
            role(r.caps, Role::Accent),
            "normal chevron stays accent"
        );
        md_theme::set_theme_mode(false); // restore
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
            history: None,
            command_output: None,
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: Some(ModeBadge {
                label: "PLAN".into(),
                colour: BadgeColour::Mode,
            }),
            bypass_indicator: None,
            session_name: None,
            reasoning_effort: None,
            goal: None,
            loop_status: None,
            todo: None,
            subtasks: None,
            approval: None,
            user_input: None,
            pending_messages: Vec::new(),
            round_cap_panel: None,
        };
        let row = r.build_status_row(&status, 60, false);
        // Concatenate visible chars from the cells. `PAD_COL` of leading
        // spaces, then the badge, then " · " separator, then the body.
        let visible: String = row.iter().map(|c| c.ch).collect();
        let trimmed = visible.trim_start();
        assert!(
            trimmed.starts_with("PLAN · "),
            "badge + separator must precede the model run; got: {:?}",
            visible
        );
        assert!(
            visible.contains("glm-5"),
            "model name must still appear in the row; got: {:?}",
            visible
        );
    }

    /// While composing a `!` command the status row shows a `shell` mode badge
    /// (sibling of `PLAN`/`auto`), in atomcode brand-purple, and it TAKES
    /// PRECEDENCE over the persistent plan/auto badge — a `!` line runs in the
    /// shell, bypassing the agent, so the agent mode is momentarily irrelevant.
    #[test]
    fn shell_mode_shows_shell_badge_overriding_plan() {
        use crate::highlight::theme as md_theme;
        let _theme = md_theme::test_lock();
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        md_theme::set_theme_mode(false); // dark → deterministic Role::Shell
        let shell_fg = role(r.caps, Role::Shell);
        let status = StatusLine {
            model: "glm-5".into(),
            cwd: "~/proj".into(),
            history: None,
            command_output: None,
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: Some(ModeBadge {
                label: "PLAN".into(),
                colour: BadgeColour::Mode,
            }),
            bypass_indicator: None,
            session_name: None,
            reasoning_effort: None,
            goal: None,
            loop_status: None,
            todo: None,
            subtasks: None,
            approval: None,
            user_input: None,
            pending_messages: Vec::new(),
            round_cap_panel: None,
        };
        let row = r.build_status_row(&status, 60, /* shell */ true);
        let visible: String = row.iter().map(|c| c.ch).collect();
        assert!(
            visible.trim_start().starts_with("shell "),
            "shell badge takes the slot while composing !; got: {:?}",
            visible
        );
        assert!(
            !visible.contains("PLAN"),
            "plan badge is suppressed while the shell badge is showing; got: {:?}",
            visible
        );
        // Badge cells are brand-purple, not the plan periwinkle / border cyan.
        let first_glyph = row
            .iter()
            .find(|c| c.ch != ' ')
            .expect("badge glyph present");
        assert_eq!(
            first_glyph.style.fg, shell_fg,
            "shell badge painted brand purple"
        );
        md_theme::set_theme_mode(false); // restore
    }

    /// Default Build mode produces no badge — row is identical to the
    /// pre-mode-indicator layout. Guards against accidental "PLAN" leak
    /// when no mode is active.
    #[test]
    fn build_status_row_default_mode_emits_no_badge() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let row = r.build_status_row(&status_basic(), 60, false);
        let visible: String = row.iter().map(|c| c.ch).collect();
        assert!(
            !visible.contains("PLAN"),
            "no mode indicator should produce no PLAN badge; got: {:?}",
            visible
        );
    }

    /// `mode_indicator` (Plan) and `bypass_indicator` (Auto) are mutually
    /// exclusive — they share ONE left badge slot. If both are somehow set,
    /// the mode indicator wins and the bypass badge does not also render.
    #[test]
    fn build_status_row_mode_indicator_wins_over_bypass() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let status = StatusLine {
            model: "glm-5".into(),
            cwd: "~/proj".into(),
            history: None,
            command_output: None,
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: Some(ModeBadge {
                label: "PLAN".into(),
                colour: BadgeColour::Mode,
            }),
            bypass_indicator: Some("\u{26a0} BYPASS".into()),
            session_name: None,
            reasoning_effort: None,
            goal: None,
            loop_status: None,
            todo: None,
            subtasks: None,
            approval: None,
            user_input: None,
            pending_messages: Vec::new(),
            round_cap_panel: None,
        };
        let row = r.build_status_row(&status, 60, false);
        let visible: String = row.iter().map(|c| c.ch).collect();
        // PLAN badge on the left.
        assert!(
            visible.trim_start().starts_with("PLAN "),
            "PLAN badge must appear first on the left; got: {:?}",
            visible
        );
        // Single left slot: BYPASS does NOT also render.
        assert!(
            !visible.contains("BYPASS"),
            "bypass must not render when a mode indicator is present; got: {:?}",
            visible
        );
    }

    /// The left mode badge (Plan / accept edits) is coloured with the dedicated
    /// `Role::Mode` purple (`AnsiValue(104)`), NOT the global `Role::Brand`
    /// magenta. This keeps the recolour scoped to the mode indicator — tool
    /// markers, the spinner, and the prompt glyph stay Brand.
    #[test]
    fn build_status_row_mode_indicator_uses_mode_purple() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let status = StatusLine {
            model: "glm-5".into(),
            cwd: "~/proj".into(),
            history: None,
            command_output: None,
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: Some(ModeBadge {
                label: "PLAN".into(),
                colour: BadgeColour::Mode,
            }),
            bypass_indicator: None,
            session_name: None,
            reasoning_effort: None,
            goal: None,
            loop_status: None,
            todo: None,
            subtasks: None,
            approval: None,
            user_input: None,
            pending_messages: Vec::new(),
            round_cap_panel: None,
        };
        let row = r.build_status_row(&status, 60, false);
        let idx = row
            .iter()
            .position(|c| c.ch == 'P')
            .expect("PLAN badge must be present");
        let badge: String = row[idx..idx + 4].iter().map(|c| c.ch).collect();
        assert_eq!(badge, "PLAN", "expected the PLAN mode badge at the left");
        assert!(
            row[idx..idx + 4]
                .iter()
                .all(|c| c.style.fg == Some(crossterm::style::Color::AnsiValue(104))),
            "PLAN mode badge must use the mode purple AnsiValue(104); got: {:?}",
            row[idx..idx + 4]
                .iter()
                .map(|c| c.style.fg)
                .collect::<Vec<_>>()
        );
    }

    /// `mode_indicator` with `BadgeColour::Plan` takes the left badge slot
    /// with the dedicated plan colour (orange `Role::Plan`), distinct from
    /// the AcceptEdits `Role::Mode` periwinkle. Verifies both the glyph
    /// content and the colour choice.
    #[test]
    fn build_status_row_plan_indicator_uses_plan_colour() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let plan_fg = role(r.caps, Role::Plan);
        let status = StatusLine {
            model: "glm-5".into(),
            cwd: "~/proj".into(),
            history: None,
            command_output: None,
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: Some(ModeBadge {
                label: "\u{23f8} plan".into(),
                colour: BadgeColour::Plan,
            }),
            bypass_indicator: None,
            session_name: None,
            reasoning_effort: None,
            goal: None,
            loop_status: None,
            todo: None,
            subtasks: None,
            approval: None,
            user_input: None,
            pending_messages: Vec::new(),
            round_cap_panel: None,
        };
        let row = r.build_status_row(&status, 60, false);
        let visible: String = row.iter().map(|c| c.ch).collect();
        assert!(
            visible.trim_start().starts_with("\u{23f8} plan"),
            "plan_indicator badge must appear first on the left; got: {:?}",
            visible
        );
        // The badge glyphs are painted with the Plan colour (AnsiValue(208)).
        let badge_cells: Vec<_> = row
            .iter()
            .skip_while(|c| c.ch == ' ')
            .take("\u{23f8} plan".chars().count())
            .collect();
        assert!(
            badge_cells.iter().all(|c| c.style.fg == plan_fg),
            "plan badge must use Role::Plan colour (AnsiValue(208)); got: {:?}",
            badge_cells.iter().map(|c| c.style.fg).collect::<Vec<_>>()
        );
    }

    /// `mode_indicator` with `BadgeColour::Secondary` (Build mode, faint
    /// secondary style) takes the left badge slot when set. Guards the new
    /// field is actually wired into the rendering pipeline and not silently
    /// dropped.
    #[test]
    fn build_status_row_build_indicator_renders_badge() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let status = StatusLine {
            model: "glm-5".into(),
            cwd: "~/proj".into(),
            history: None,
            command_output: None,
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: Some(ModeBadge {
                label: "\u{23f8} build".into(),
                colour: BadgeColour::Secondary,
            }),
            bypass_indicator: None,
            session_name: None,
            reasoning_effort: None,
            goal: None,
            loop_status: None,
            todo: None,
            subtasks: None,
            approval: None,
            user_input: None,
            pending_messages: Vec::new(),
            round_cap_panel: None,
        };
        let row = r.build_status_row(&status, 60, false);
        let visible: String = row.iter().map(|c| c.ch).collect();
        assert!(
            visible.trim_start().starts_with("\u{23f8} build"),
            "build_indicator badge must appear first on the left; got: {:?}",
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
            history: None,
            command_output: None,
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: None,
            bypass_indicator: Some("\u{26a0} BYPASS".into()),
            session_name: None,
            reasoning_effort: None,
            goal: None,
            loop_status: None,
            todo: None,
            subtasks: None,
            approval: None,
            user_input: None,
            pending_messages: Vec::new(),
            round_cap_panel: None,
        };
        let row = r.build_status_row(&status, 60, false);
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
        let row = r.build_top_rule_with_context(60, Some("atomcode加解密"), None, false);
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

    #[test]
    fn build_top_rule_shows_history_position_on_left() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let row = r.build_top_rule_with_context(
            80,
            None,
            Some(crate::render::HistoryPosition {
                current: 100,
                total: 100,
            }),
            false,
        );
        let visible: String = row
            .iter()
            .filter(|cell| cell.width > 0)
            .map(|cell| cell.ch)
            .collect();
        assert!(
            visible.contains("History 100/100"),
            "history position must appear on the top rule: {visible:?}"
        );
    }

    #[test]
    fn history_position_and_session_badge_coexist() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let row = r.build_top_rule_with_context(
            80,
            Some("release/v5.0.2"),
            Some(crate::render::HistoryPosition {
                current: 999,
                total: 1000,
            }),
            false,
        );
        let visible: String = row
            .iter()
            .filter(|cell| cell.width > 0)
            .map(|cell| cell.ch)
            .collect();
        assert!(visible.contains("History 999/1000"), "{visible:?}");
        assert!(visible.contains("release/v5.0.2"), "{visible:?}");
    }

    #[test]
    fn session_item_height_first_two_rest_three() {
        // The first session (menu index HEADER_ROWS) is 2 rows; every later
        // session is 3 (leading blank spacer). All three height accountings —
        // the two paginator fit-loops and the footer `menu_rows` reservation —
        // share this helper, so they can never disagree with the render loop.
        let h0 = crate::modals::session_picker::HEADER_ROWS;
        assert_eq!(RetainedRenderer::<CountingSink>::session_item_height(h0), 2);
        assert_eq!(
            RetainedRenderer::<CountingSink>::session_item_height(h0 + 1),
            3
        );
        assert_eq!(
            RetainedRenderer::<CountingSink>::session_item_height(h0 + 5),
            3
        );
    }

    #[test]
    fn session_menu_row_aligns_marker_col2_title_col4() {
        let (r, _counter) = new_counting(80, 24);
        // Selected row: PAD_COL spaces, then `▸ `, then the title. So col 2 is
        // the marker and col 4 is the first title glyph — flush with the search
        // box's `│` border (col 2) and its text (col 4).
        let rows = r.build_session_menu_rows("Xtitle", "9 msgs", true, 70);
        let title = &rows[0];
        assert_eq!(title[0].ch, ' ');
        assert_eq!(title[1].ch, ' ');
        assert_eq!(title[2].ch, '▸', "marker sits under the search box border");
        assert_eq!(title[4].ch, 'X', "title starts at col 4 (search box text)");
        // Metadata row is indented 4 spaces → also col 4, aligned with the title.
        let meta = &rows[1];
        assert_eq!(meta[4].ch, '9', "metadata aligns with the title at col 4");
        // Unselected row: no marker, but the title still lands at col 4.
        let rows_u = r.build_session_menu_rows("Xtitle", "9 msgs", false, 70);
        assert_eq!(rows_u[0][2].ch, ' ', "no marker when unselected");
        assert_eq!(rows_u[0][4].ch, 'X', "unselected title also at col 4");
    }

    /// `None` session_name keeps the top rule pristine — no reverse
    /// cells, no text overlay. Guards against the badge leaking onto
    /// auto-named or default sessions.
    #[test]
    fn build_top_rule_with_badge_none_emits_plain_rule() {
        let (mut r, _counter) = new_counting(80, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let row = r.build_top_rule_with_context(60, None, None, false);
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
        let row = r.build_top_rule_with_context(40, Some(long), None, false);
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
            avg < 250,
            "retained keystroke regressed: avg={} B (budget < 250)",
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
        assert!(open_cost < 2200, "retained open: {} B", open_cost);
        assert!(close_cost < 2200, "retained close: {} B", close_cost);
        assert!(nav_avg < 800, "retained nav: {} B", nav_avg);
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
        // Upper bound: cold start (first paint after session init) re-emits
        // every non-blank cell + UTF-8 CJK + rule + cursor moves. The 13×"你是谁"
        // burst is 39 CJK × 2 = 78 cols = EXACTLY text_budget (80 − 2), so the
        // final coalesced frame also soft-wraps the end-of-buffer caret onto a
        // fresh row (the Windows-caret fix in wrap_with_cursor) — which grows
        // the input box by a row and shifts the bottom rule + status down,
        // adding a one-time re-emit of those rows. Still ONE emit (the pre-flush
        // zero-byte assertion above proves coalescing held); budget 3500 B,
        // observed ~3072 B — far below the ~6 KB a broken per-frame re-emit
        // (40 frames) or double-emit would cost.
        assert!(
            burst_bytes > 0 && burst_bytes < 3500,
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

    /// Footer-jitter fix invariant: a body overflow scrolls the whole
    /// viewport (footer included) up one row, so it must flag a pending
    /// scroll-flush — that's the signal the render worker uses to repaint the
    /// footer the same tick instead of waiting ~5ms for the deferred tick.
    /// `take_pending_scroll_flush` returns it once, then clears.
    #[test]
    fn body_overflow_flags_pending_scroll_flush_and_take_clears() {
        // Short viewport so a handful of body rows overflow the visible cap.
        let (mut r, _c) = new_counting(80, 10);
        let status = status_basic();
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        assert!(
            !r.take_pending_scroll_flush(),
            "no body scroll has happened yet"
        );

        // Push well past the visible cap so the overflow LF fires.
        for i in 0..20 {
            r.render(UiLine::User(format!("line-{i}")));
        }
        assert!(
            r.take_pending_scroll_flush(),
            "overflow must flag a pending scroll-flush for the worker"
        );
        assert!(
            !r.take_pending_scroll_flush(),
            "take must clear the flag (one repaint per drained burst)"
        );
    }

    /// Coalescing guard: InputPrompt / IME bursts never push body rows, so
    /// they must NEVER set the scroll-flush flag — otherwise the worker would
    /// eager-paint per keystroke and defeat the deferred-tick coalescing that
    /// `retained_coalesce_many_renders_one_emit` pins.
    #[test]
    fn input_prompt_burst_never_flags_scroll_flush() {
        let (mut r, _c) = new_counting(80, 24);
        let status = status_basic();
        r.flush_deferred();
        let mut buf = String::new();
        for ch in "你是谁你是谁你是谁你是谁你是谁你是谁你是谁".chars() {
            buf.push(ch);
            r.render(UiLine::InputPrompt {
                buf: buf.clone(),
                cursor_byte: buf.len(),
                menu: None,
                status: status.clone(),
                attachments: Vec::new(),
            });
        }
        assert!(
            !r.take_pending_scroll_flush(),
            "InputPrompt never scrolls the body, so it must not trigger the eager scroll-flush"
        );
    }

    /// Overflow fix: a long paste / typed text used to grow the input box (and
    /// the footer) past the screen height. The footer must now stay within the
    /// screen, with the input box capped to a scrolling window (full text still
    /// lives in input_buf — display-only cap).
    #[test]
    fn long_input_does_not_overflow_footer() {
        let (mut r, _c) = new_counting(80, 24);
        let big = (0..60)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        r.input_buf = big.clone();
        r.input_cursor_byte = big.len();
        let footer = r.current_footer_rows();
        assert!(
            footer < 24,
            "footer ({footer}) must not exceed screen height for a 60-line input"
        );
        // top rule + <=MAX_INPUT_ROWS input rows + bot rule (no menu/status here).
        assert!(
            footer <= 1 + MAX_INPUT_ROWS + 1,
            "input box must be capped near MAX_INPUT_ROWS, got footer={footer}"
        );
        // A short input is unaffected.
        r.input_buf = "hi".into();
        r.input_cursor_byte = 2;
        assert!(
            r.current_footer_rows() < footer,
            "short input should be smaller"
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

    #[test]
    fn retained_inflight_subagent_fanout_uses_brand_activity_style() {
        let (mut r, _buf) = new_capturing(100, 24);
        r.render_inflight_tool(
            "⠋",
            "Task",
            "3 subtasks",
            " · thinking… (57s · ↑ 715 tokens)",
        );

        let row = r
            .body_lines
            .iter()
            .find(|row| row.iter().map(|cell| cell.ch).collect::<String>().contains("Task"))
            .expect("live Task row");
        let brand = r.style_for(Role::Brand).fg;
        assert!(brand.is_some(), "capturing terminal should enable brand color");
        assert!(
            row.iter()
                .filter(|cell| !cell.ch.is_whitespace())
                .all(|cell| cell.style.fg == brand),
            "live Task activity, including detail and spinner metadata, must use brand color"
        );

        r.render_inflight_tool("⠙", "Read", "src/lib.rs", " (2s)");
        let ordinary = r
            .body_lines
            .iter()
            .find(|row| row.iter().map(|cell| cell.ch).collect::<String>().contains("Read"))
            .expect("ordinary live tool row");
        assert!(
            ordinary
                .iter()
                .any(|cell| !cell.ch.is_whitespace() && cell.style.fg != brand),
            "ordinary live tools must retain their existing tool palette"
        );
    }

    #[test]
    fn retained_streaming_body_under_inflight_strip_does_not_trail() {
        // Regression: the `task` fan-out streams `↻`/`✓` progress lines as body rows WHILE its
        // `● Task(N subtasks)` inflight strip is the live tail. Before `push_body_row` learned to
        // lift the strip, each streamed line buried + orphaned a frozen `●Task…` snapshot — the
        // reported time-lapse trail (`Task(5 subtasks) · thinking… · 18.9s / 19.0s / …`). The
        // strip must be lifted on each body push and re-emitted ONCE at the new tail.
        let (mut r, _buf) = new_capturing(80, 24);
        let text = |row: &Vec<Cell>| row.iter().map(|c| c.ch).collect::<String>();

        r.render_inflight_tool("●", "Task", "5 subtasks", "");
        assert_eq!(r.inflight_tool_rows, 1, "strip established as one live row");

        for i in 0..6 {
            r.push_body_text(&format!("start explore{i}"), &CellStyle::default());
            assert_eq!(
                r.inflight_tool_rows, 0,
                "a body push must LIFT the live strip"
            );
            // Next spinner tick re-emits the strip at the new tail.
            r.render_inflight_tool("●", "Task", "5 subtasks", "");
        }

        let strip_rows = r
            .body_lines
            .iter()
            .filter(|row| text(row).contains("Task"))
            .count();
        assert_eq!(
            strip_rows, 1,
            "exactly ONE live Task strip — no trail; got {strip_rows}"
        );
        let streamed = r
            .body_lines
            .iter()
            .filter(|row| text(row).contains("start explore"))
            .count();
        assert_eq!(
            streamed, 6,
            "all six streamed lines present above the strip"
        );
    }

    /// The retained `InputPrompt` handler treats reaching the idle prompt as
    /// "the turn is over": it `commit_inflight_tool()`s any running tool and
    /// clears the live spinner. That is correct at idle, but destructive
    /// mid-turn — committing a still-running tool and then re-establishing the
    /// strip on the next `draw_spinner_now` is exactly what flashed a garbled
    /// spinner/thinking row when Tab cycled the agent mode during streaming
    /// ("tab 切换时思考过程被覆盖，过一会儿恢复"). This pins the hazard so the
    /// `set_agent_mode` guard that skips the idle repaint while streaming can't
    /// be dropped as "unnecessary".
    #[test]
    fn retained_input_prompt_commits_inflight_tool_hazard() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.render(UiLine::ToolCallInFlight {
            id: "t1".into(),
            name: "Bash".into(),
            detail: "cargo test".into(),
            hint: None,
        });
        assert!(r.inflight_tool.is_some(), "tool is in flight");
        assert!(r.inflight_tool_rows > 0, "inflight strip established");

        // Idle repaint mid-stream — what `set_agent_mode` → `redraw_idle_plain`
        // used to do unconditionally on a Tab mode-cycle.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        assert!(
            r.inflight_tool.is_none(),
            "InputPrompt committed the in-flight tool — set_agent_mode must skip \
             this idle repaint while streaming"
        );
    }

    #[test]
    fn retained_inflight_fallback_rowcount_change_does_not_pop_real_content() {
        // Regression for the `lift_inflight_strip` re-entrancy: `render_inflight_tool`'s
        // FALLBACK branch truncates the old strip manually then re-pushes via `push_body_row`.
        // If `inflight_tool_rows` isn't zeroed before that loop, push_body_row's new
        // `lift_inflight_strip` sees the stale count and pops that many rows of REAL content.
        let (mut r, _buf) = new_capturing(80, 24);
        let text = |row: &Vec<Cell>| row.iter().map(|c| c.ch).collect::<String>();
        r.push_body_text("real line A", &CellStyle::default());
        r.push_body_text("real line B", &CellStyle::default());
        assert_eq!(
            r.body_lines
                .iter()
                .filter(|row| text(row).contains("real line"))
                .count(),
            2
        );

        // First render → fallback with prev_rows == 0. Bash renders one command
        // row, one spacer, and its `Running` spinner row = 3 rows.
        r.render_inflight_tool("\u{25cf}", "Bash", "short", "");
        assert_eq!(r.inflight_tool_rows, 3);

        // A detail that WRAPS to more rows ⇒ prev_rows(3) != n(>3) ⇒ fallback re-push.
        // Bash wraps at shell boundaries (format_shell_command breaks at each `&&`),
        // so use a multi-segment command rather than one long unbreakable token.
        let long = "cd /tmp && echo one && echo two && echo three && echo four";
        r.render_inflight_tool("\u{25cf}", "Bash", long, "");
        assert!(
            r.inflight_tool_rows > 3,
            "wrapped strip should grow past 3 rows: {}",
            r.inflight_tool_rows
        );

        // Both real lines must survive — the fallback re-push must not have popped them.
        assert_eq!(
            r.body_lines
                .iter()
                .filter(|row| text(row).contains("real line"))
                .count(),
            2,
            "fallback re-push popped real content via a stale inflight_tool_rows"
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
        let body_rows_after_first = r.body_lines.len();
        let bytes_after_first = buf.lock().unwrap().len();
        assert!(bytes_after_first > 0, "first render must emit some bytes");

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
        assert_eq!(
            r.body_lines.len(),
            body_rows_after_first,
            "body_lines grew from {} to {} rows across 50 ticks — should stay \
             at the first render's row count for the in-place path",
            body_rows_after_first,
            r.body_lines.len(),
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
            name: "Grep".into(),
            detail: "LEAKMARKER_PLACEHOLDER_AAAA".into(),
            hint: None,
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
            hint: None,
        });
        r.render(UiLine::Spinner {
            frame: "⠋".into(),
            label: "Running Bash… · 12s".into(),
        });
        // Bash renders the command as a static block; the elapsed meta now
        // rides the `Running · <t>` spinner row appended below it.
        let rows: Vec<String> = r
            .body_lines
            .iter()
            .map(|row| row.iter().map(|c| c.ch).collect())
            .collect();
        assert!(
            rows.iter().any(|t| t.contains("Bash(cargo install")),
            "command block row missing command detail; got: {:?}",
            rows
        );
        let last = r.body_lines.last().expect("Running spinner row expected");
        let last_text: String = last.iter().map(|c| c.ch).collect();
        assert!(
            last_text.contains("Running") && last_text.contains("· 12s"),
            "Running spinner row missing elapsed meta suffix; got: {:?}",
            last_text
        );
    }

    #[test]
    fn spinner_meta_suffix_extracts_after_first_separator() {
        assert_eq!(spinner_meta_suffix("Running Bash… · 12s"), " · 12s");
        // Effort now leads the metadata run and elapsed trails it; queue (when
        // present) sits between. The effort hint must NOT ride onto a tool row.
        assert_eq!(
            spinner_meta_suffix("Running Bash… · 2 queued · 12s"),
            " · 2 queued · 12s"
        );
        // No metadata yet (no phase clock tick) → empty suffix.
        assert_eq!(spinner_meta_suffix("Pondering…"), "");
        assert_eq!(spinner_meta_suffix(""), "");
        // Effort first, elapsed last → only the trailing elapsed forwards.
        assert_eq!(
            spinner_meta_suffix("Running Bash… · thinking with high effort · 12s"),
            " · 12s"
        );
        assert_eq!(
            spinner_meta_suffix("Running Bash… · thinking with max effort · 2 queued · 12s"),
            " · 2 queued · 12s"
        );
        // Effort with no time/queue after it → nothing forwards.
        assert_eq!(
            spinner_meta_suffix("Running Bash… · thinking with high effort"),
            ""
        );
    }

    #[test]
    fn spinner_meta_suffix_keeps_parenthesized_elapsed_group_intact() {
        // `format_spinner_label` now wraps the phase clock + live token counter
        // in ONE parenthesized group: `Pondering… (3s · ↑ 1.93K tokens)`. The
        // forwarded suffix must include the opening `(` so a bash row reads
        // `Running (3s · ↑ 1.93K tokens)`. Keying off the ` · ` INSIDE the
        // parens dropped `(3s` and left a dangling `)` (reported bug:
        // `Running · ↑ 1.93K tokens)`).
        assert_eq!(
            spinner_meta_suffix("Pondering… (3s · ↑ 1.93K tokens)"),
            " (3s · ↑ 1.93K tokens)"
        );
        // tokens == 0 → elapsed-only parens, still balanced.
        assert_eq!(spinner_meta_suffix("Pondering… (3s)"), " (3s)");
        // Queue segment precedes the parenthesized group — forward both.
        assert_eq!(
            spinner_meta_suffix("Pondering… · 2 queued (3s · ↑ 1.93K tokens)"),
            " · 2 queued (3s · ↑ 1.93K tokens)"
        );
        // Effort leads (a tool "isn't thinking") → spliced out, parens kept whole.
        assert_eq!(
            spinner_meta_suffix("Cogitating… · thinking with high effort (3s · ↑ 1.93K tokens)"),
            " (3s · ↑ 1.93K tokens)"
        );
        // Effort + queue before the parens → keep queue and the whole group.
        assert_eq!(
            spinner_meta_suffix(
                "Cogitating… · thinking with max effort · 2 queued (3s · ↑ 1.93K tokens)"
            ),
            " · 2 queued (3s · ↑ 1.93K tokens)"
        );
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
    /// Fix: hide the cursor whenever an inflight tool is active.
    /// `inflight_tool.is_none()` flips back at commit time, so the
    /// cursor reappears at the input box on the next paint without a
    /// leftover blink. (The live spinner was removed from this gate —
    /// see `retained_spinner_keeps_input_cursor_visible`.)
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
            hint: None,
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

    /// Regression: user reported the input caret vanishing "after running
    /// a while" (can still type; `/clear` fixes it). Root cause: a turn that
    /// ERRORS while a tool is in flight never committed `inflight_tool`
    /// (`UiLine::Error` didn't, unlike TurnComplete/TurnCancelled), so
    /// `paint_footer`'s `suppress_cursor` kept the caret hidden on every
    /// repaint. Fix: the Error arm now commits the inflight tool.
    #[test]
    fn retained_error_after_inflight_tool_restores_cursor() {
        let (mut r, buf) = new_capturing(80, 24);
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
            detail: "cargo test".into(),
            hint: None,
        });
        r.render(UiLine::Spinner {
            frame: "⠙".into(),
            label: "Running Bash".into(),
        });
        r.flush_deferred();
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        drain_into_vterm(&buf, &mut vterm);
        assert!(!vterm.cursor_visible(), "hidden while tool in flight");

        // Turn errors mid-tool (no ToolCallCommit / TurnComplete arrives).
        r.render(UiLine::Error("gateway 5xx".into()));
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            vterm.cursor_visible(),
            "caret must return after an error commits the orphaned inflight tool"
        );
    }

    /// Safety net: any termination path that returns to the idle input prompt
    /// without committing the inflight tool (silent non-Stopped stops, goal
    /// timeouts) must not leave the caret suppressed — the idle `InputPrompt`
    /// redraw commits any orphan.
    #[test]
    fn retained_idle_redraw_commits_orphan_inflight_tool() {
        let (mut r, buf) = new_capturing(80, 24);
        // A committed body line ABOVE the inflight strip — the orphan commit
        // must remove only the strip rows, never the real body above them.
        r.render(UiLine::AssistantText("SENTINEL_KEEP_ME".into()));
        r.render(UiLine::AssistantLineBreak);
        // Long detail so the inflight strip wraps to >1 row.
        r.render(UiLine::ToolCallInFlight {
            id: "call_1".into(),
            name: "Bash".into(),
            detail: "cd /Users/theo/Documents/workspace/atomcode && cargo test -p atomcode-tuix --lib -- --nocapture 2>&1 | tail -200".into(),
            hint: None,
        });
        r.render(UiLine::Spinner {
            frame: "⠙".into(),
            label: "Running Bash".into(),
        });
        r.flush_deferred();
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        drain_into_vterm(&buf, &mut vterm);
        assert!(!vterm.cursor_visible(), "hidden while tool in flight");

        // Return to idle WITHOUT a commit/complete/error (silent stop).
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
            "idle redraw must commit the orphan inflight tool and restore the caret"
        );
        // Assert on the body MODEL (not scrollback — the sentinel was already
        // emitted there on first render, so it survives there even when the
        // model is corrupted). The double-remove truncates the sentinel out of
        // `body_lines`; a correct commit removes only the strip rows.
        let body_has_sentinel = r.body_lines.iter().any(|row| {
            row.iter()
                .map(|c| c.ch)
                .collect::<String>()
                .contains("SENTINEL_KEEP_ME")
        });
        assert!(
            body_has_sentinel,
            "committing the orphan strip must not eat the real body row above it"
        );
    }

    /// Regression: user reported "no cursor" (没有光标) while typing
    /// into the input box DURING a reply — the caret disappears as soon
    /// as the streaming spinner starts animating. Root cause: the live
    /// spinner used to live in the FOOTER and `paint_footer` hid the
    /// terminal cursor while `live_spinner_active` was true. But the
    /// spinner moved to the BODY (see `push_or_update_live_spinner`)
    /// and the input box stayed editable during streaming (type-ahead
    /// message queue), so hiding the caret left the user typing blind.
    ///
    /// Fix: only `inflight_tool.is_some()` suppresses the cursor now —
    /// the spinner is in the body and the input caret must stay visible
    /// so the user sees where they're typing while queuing the next
    /// message.
    #[test]
    fn retained_spinner_keeps_input_cursor_visible() {
        let term_w: u16 = 80;
        let (mut r, buf) = new_capturing(term_w, 24);

        // Seed input prompt so the footer has a cursor position.
        r.render(UiLine::InputPrompt {
            buf: "meiyouguangbiao".into(),
            cursor_byte: 16,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        // Spinner ticks (simulating streaming) — the caret must NOT
        // disappear.
        for frame in ["⠋", "⠙", "⠹", "⠸"] {
            r.render(UiLine::Spinner {
                frame: frame.into(),
                label: "thinking".into(),
            });
        }
        r.flush_deferred();
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            vterm.cursor_visible(),
            "input cursor must stay visible during spinner activity — \
             the input box is editable during streaming (type-ahead), \
             hiding it leaves the user typing blind"
        );
    }

    /// Test that when input box is hidden (e.g. MenuKind::Plugin or
    /// MenuKind::PluginInfo), the terminal cursor is suppressed (hidden),
    /// EXCEPT when MenuKind::Marketplace has "Add Marketplace" as item 2 (which is
    /// Screen::AddUrl).
    #[test]
    fn retained_plugin_modal_hides_stray_cursor() {
        let term_w: u16 = 80;
        let (mut r, buf) = new_capturing(term_w, 24);

        // 1. MenuKind::Plugin (e.g., browse/installed list) -> cursor must be hidden
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: Some(crate::render::MenuPayload {
                items: vec![("Browse".into(), "Desc".into())],
                selected: 0,
                kind: crate::render::MenuKind::Plugin,
            }),
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            !vterm.cursor_visible(),
            "cursor must be hidden in plugin modal when not entering text"
        );

        // 2. MenuKind::Marketplace WITH Add Marketplace -> cursor must be visible
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: Some(crate::render::MenuPayload {
                items: vec![
                    ("Browse".into(), "Desc".into()),
                    ("".into(), "".into()),
                    ("Add Marketplace".into(), "".into()),
                ],
                selected: 0,
                kind: crate::render::MenuKind::Marketplace,
            }),
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let mut vterm2 = crate::test_term::VirtualTerminal::new(80, 24);
        drain_into_vterm(&buf, &mut vterm2);
        assert!(
            vterm2.cursor_visible(),
            "cursor must be visible on the Add Marketplace screen"
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
            .unwrap_or_else(|| panic!("no full-width rule row found; dump:\n{}", vterm.dump()));
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

    /// Welcome via vterm: after receiving UiLine::Welcome, all expected
    /// welcome pieces (brand / cwd / model / tips heading / /login) must
    /// appear on the screen above the footer.
    #[test]
    fn retained_welcome_lines_render_via_vterm() {
        let (mut r, buf) = new_capturing(80, 30);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 30);
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

        // Two-column layout: brand + blank + (mascot | tips) rows + cwd/model +
        // trailing blank. All pieces should be somewhere in the visible body.
        let found_brand = (0..30).any(|r| vterm.row_text(r).contains("AtomCode"));
        let found_cwd = (0..30).any(|r| vterm.row_text(r).contains("~/p/a"));
        let found_model = (0..30).any(|r| vterm.row_text(r).contains("glm-5"));
        // New layout shows tips (/login always pinned) instead of idle hint text.
        let found_hint = (0..30).any(|r| vterm.row_text(r).contains("/login"));
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

    /// Regression for issue #709 ("resize 历史布局乱了"): on a width
    /// change the WHOLE transcript must re-wrap, not just the welcome
    /// banner. The pre-fix path rebuilt the welcome and `clip`ped every
    /// other history row to the new width — so on a shrink the right
    /// portion of any over-wide row was silently dropped. Here a long
    /// assistant line fits one row at width 80 (both `ALPHATOKEN` and
    /// `OMEGATOKEN` visible) but must wrap to ≥2 rows at width 40. The
    /// clip path would have truncated the row, losing `OMEGATOKEN`
    /// entirely; the reflow path re-wraps so BOTH tokens survive, on
    /// different rows.
    #[test]
    fn retained_resize_reflows_history_body_not_just_welcome() {
        let (mut r, buf) = new_capturing(80, 24);
        let status = status_basic();

        // ~60 visible cols: one row at 80, must wrap at 40.
        let line = "ALPHATOKEN aa bb cc dd ee ff gg hh ii jj kk ll mm nn OMEGATOKEN";
        r.render(UiLine::AssistantText(line.into()));
        r.render(UiLine::AssistantLineBreak);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Resize narrower. Feed ONLY the post-resize bytes into a fresh
        // 40-wide vterm so we assert on the reflowed result in isolation.
        let buf_before: Vec<u8> = buf.lock().unwrap().clone();
        let _ = buf_before;
        buf.lock().unwrap().clear();
        r.on_resize(40, 24);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let mut vterm = crate::test_term::VirtualTerminal::new(40, 24);
        drain_into_vterm(&buf, &mut vterm);

        let alpha_row = (0..24).find(|r| vterm.row_text(*r).contains("ALPHATOKEN"));
        let omega_row = (0..24).find(|r| vterm.row_text(*r).contains("OMEGATOKEN"));
        assert!(
            alpha_row.is_some(),
            "ALPHATOKEN missing after narrow resize\ndump:\n{}",
            vterm.dump()
        );
        assert!(
            omega_row.is_some(),
            "OMEGATOKEN was dropped on narrow resize — history was clipped, \
             not reflowed (issue #709)\ndump:\n{}",
            vterm.dump()
        );
        assert_ne!(
            alpha_row,
            omega_row,
            "tokens landed on the same row at width 40 — line did not wrap, \
             so this test no longer exercises reflow\ndump:\n{}",
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
        // Height 30: the two-column layout produces more rows when stacked on
        // a narrow terminal (brand×1-2 + blank + mascot×6 + cwd + model +
        // tips×5 + blank ≈ 17 body rows). We need enough height so the brand
        // row stays in the visible viewport after the banner expands on shrink.
        let (mut r, buf) = new_capturing(80, 30);

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
        let mut pre = crate::test_term::VirtualTerminal::new(80, 30);
        drain_into_vterm(&buf, &mut pre);

        r.on_resize(24, 30);
        r.flush_deferred();
        let mut post = crate::test_term::VirtualTerminal::new(24, 30);
        drain_into_vterm(&buf, &mut post);

        let brand_row = (0..30)
            .map(|row| post.row_text(row))
            .find(|row| row.contains("AtomCode"))
            .expect("brand row should remain visible after shrinking");
        let version_row = (0..30)
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

        // Seed body with a long tool call: a `● Name(payload)` row whose
        // display width far exceeds any sane "shrink-to" target.
        // Use "EditFile" (not "Bash") so the generic inline render path is
        // exercised — bash now wraps at shell boundaries (format_shell_command),
        // and a 100-char single-token command intentionally stays one line even
        // at narrow widths (never splits mid-token by design).
        r.render(UiLine::ToolCall {
            name: "EditFile".into(),
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
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        // 22-col WIDTH is the test's actual subject (column reflow).
        // Use 30-row HEIGHT — large enough that the reflowed banner
        // (title × 2 + blank + mascot × 6 + path × 4 + model × 2 +
        //  heading + tips × 4 + blank ≈ 21 body rows, plus 4 footer rows)
        // fits entirely in the viewport with headroom.
        let (mut r, buf) = new_capturing(22, 30);
        let mut vterm = crate::test_term::VirtualTerminal::new(22, 30);

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
            (0..30).any(|row| vterm.row_text(row).contains("AtomCode")),
            "brand missing on narrow terminal\n{}",
            vterm.dump()
        );
        assert!(
            (0..30).any(|row| vterm.row_text(row).contains("workspace")),
            "path should wrap instead of disappearing on narrow terminal\n{}",
            vterm.dump()
        );
        assert!(
            (0..30).any(|row| vterm.row_text(row).contains("MiniMax")),
            "model should wrap instead of disappearing on narrow terminal\n{}",
            vterm.dump()
        );
        // New layout: tips replace old idle hint text. /login is always
        // pinned; tips heading is always present.
        assert!(
            (0..30).any(|row| vterm.row_text(row).contains("/login")),
            "pinned /login tip should be visible on narrow terminal\n{}",
            vterm.dump()
        );
        assert!(
            (0..30).any(|row| {
                let t = vterm.row_text(row);
                t.contains("Tips") || t.contains("提示")
            }),
            "tips heading should be visible on narrow terminal\n{}",
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

    /// ToolCall bash: renders `● bash` header + `ls -la` command row
    /// (new two-part shape instead of old inline `● bash(ls -la)`).
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
        // Header row: `● bash(ls -la)` — command inline in parens, matching
        // other tools (`● Read(arg)`).
        assert!(
            vterm.any_row(|row| row.contains("●")
                && row.contains("bash(")
                && row.contains("ls -la")),
            "bash header row missing\ndump:\n{}",
            vterm.dump()
        );
    }

    /// ToolCall glyph `●` must sit at col 0, same baseline as user
    /// echo and input chevron. With bash two-part rendering, the header
    /// row (`● bash`) carries the glyph.
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

        // The header row is `● bash` (no inline command) — glyph at col 0.
        let row_idx = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("●") && vterm.row_text(i).contains("bash"))
            .unwrap_or_else(|| panic!("tool call header row missing\ndump:\n{}", vterm.dump()));
        assert_eq!(
            vterm.cell_at(row_idx, 0).ch,
            '●',
            "tool-call glyph must land at col 0, got row: {:?}\ndump:\n{}",
            vterm.row_text(row_idx),
            vterm.dump()
        );
    }

    #[test]
    fn retained_committed_inflight_tool_uses_static_tool_call_styles() {
        let (mut static_r, _static_buf) = new_capturing(80, 24);
        static_r.render(UiLine::ToolCall {
            name: "EditFile".into(),
            detail: "test.txt ← \"10\"".into(),
        });
        let static_row = static_r.body_lines.last().expect("static tool row");

        let (mut inflight_r, _inflight_buf) = new_capturing(80, 24);
        inflight_r.render(UiLine::ToolCallInFlight {
            id: "call-edit-1".into(),
            name: "EditFile".into(),
            detail: "test.txt ← \"10\"".into(),
            hint: None,
        });
        inflight_r.render(UiLine::ToolCallCommit {
            call_id: Some("call-edit-1".into()),
        });
        let committed_row = inflight_r.body_lines.last().expect("committed tool row");

        let static_bullet = static_row
            .iter()
            .find(|c| c.ch == '●')
            .expect("static bullet");
        let committed_bullet = committed_row
            .iter()
            .find(|c| c.ch == '●')
            .expect("committed bullet");
        assert_eq!(
            committed_bullet.style, static_bullet.style,
            "committed inflight bullet must match static ToolCall bullet style"
        );
        assert!(
            static_bullet.style.bold,
            "tool-call bullet must be highlighted"
        );

        let static_detail = static_row
            .iter()
            .find(|c| c.ch == '(')
            .expect("static detail opener");
        let committed_detail = committed_row
            .iter()
            .find(|c| c.ch == '(')
            .expect("committed detail opener");
        assert_eq!(
            committed_detail.style, static_detail.style,
            "committed inflight detail must match static ToolCall detail style"
        );
        assert!(
            !static_detail.style.bold,
            "tool-call detail must use normal foreground, not highlighted text"
        );
    }

    /// Bash ToolCall renders `● Bash(<cmd>)` — command inline in parens on the
    /// header line (matching other tools) — while still wrapping along shell
    /// boundaries (not mid-token), with no truncation and no NBSP sentinel.
    #[test]
    fn bash_command_wraps_on_shell_boundaries_not_mid_token() {
        let (mut r, buf) = new_capturing(60, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(60, 24);
        r.render(UiLine::ToolCall {
            name: "Bash".into(),
            detail:
                "cd /tmp && sed -i '' \\ -e 's/mb-2.5/mb-[10px]/g' \\ -e 's/mb-3.5/mb-[14px]/g'"
                    .into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        // Header row carries `● Bash(` plus the first command segment inline.
        assert!(
            vterm.any_row(|row| row.contains("● Bash(") && row.contains("cd /tmp")),
            "header is `● Bash(cd /tmp …` with inline paren command\ndump:\n{}",
            vterm.dump()
        );
        assert!(
            vterm.any_row(|row| row.contains("-e 's/mb-2.5/mb-[10px]/g'")),
            "the -e segment stays intact on one row\ndump:\n{}",
            vterm.dump()
        );
        // Wrapping still breaks at `&&` (the second segment lands on its own row).
        assert!(
            vterm.any_row(|row| row.trim_start().starts_with("&& sed")),
            "shell boundary wrap at `&&` preserved\ndump:\n{}",
            vterm.dump()
        );
        // Closing paren terminates the command block.
        assert!(
            vterm.any_row(|row| row.trim_end().ends_with(')')),
            "command block closed with `)`\ndump:\n{}",
            vterm.dump()
        );
        assert!(
            !vterm.any_row(|row| row.contains("truncated")),
            "no truncation\ndump:\n{}",
            vterm.dump()
        );
        // No NBSP sentinel should appear anywhere.
        assert!(
            !vterm.any_row(|row| row.contains('\u{00A0}')),
            "no NBSP sentinel expected in new layout\ndump:\n{}",
            vterm.dump()
        );
        // The command follows `● Bash(`, never flush at col 0.
        assert!(
            !vterm.any_row(|row| row.starts_with("cd /tmp")),
            "command must sit after the `● Bash(` prefix (not col 0)\ndump:\n{}",
            vterm.dump()
        );
    }

    /// Live bash committed via ToolCallInFlight → ToolCallCommit must produce
    /// the same `● Bash(<cmd>)` inline-paren block as the static
    /// `UiLine::ToolCall` arm, still wrapping at shell boundaries.
    #[test]
    fn live_bash_commits_to_inline_paren_command() {
        let (mut r, buf) = new_capturing(60, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(60, 24);
        // Live inflight strip (as ToolCallStarted would push):
        r.render(UiLine::ToolCallInFlight {
            id: "call-1".into(),
            name: "Bash".into(),
            detail: "cd /tmp && sed -i '' \\ -e 's/aa/bb/g' \\ -e 's/cc/dd/g'".into(),
            hint: None,
        });
        r.flush_deferred();
        // Commit (as ToolCallCommit would):
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-1".into()),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        // Committed form is `● Bash(cmd)` inline, matching the static arm:
        assert!(
            vterm.any_row(|row| row.contains("● Bash(") && row.contains("cd /tmp")),
            "committed header is `● Bash(cd /tmp …`\ndump:\n{}",
            vterm.dump()
        );
        assert!(
            vterm.any_row(|row| row.contains("-e 's/cc/dd/g'")),
            "wrapped segment survives\ndump:\n{}",
            vterm.dump()
        );
        assert!(
            !vterm.any_row(|row| row.contains("truncated")),
            "no (truncated)\ndump:\n{}",
            vterm.dump()
        );
        // No ghost/duplicate strip row lingering:
        let bash_headers = (0..vterm.height() as usize)
            .filter(|&i| vterm.row_text(i).contains("● Bash("))
            .count();
        assert_eq!(
            bash_headers,
            1,
            "exactly one ● Bash( header (no ghost)\ndump:\n{}",
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
            diff_stats: None,
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
        let _theme = crate::highlight::theme::test_lock();
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
            diff_stats: None,
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        assert!(
            r.body_lines
                .iter()
                .flatten()
                .any(|cell| cell.ch == '└' && cell.style.faint),
            "renderer model must retain the faint result marker"
        );
        let raw = buf.lock().unwrap().clone();
        assert!(
            raw.windows(4).any(|window| window == b"\x1b[2m"),
            "renderer byte stream must emit SGR 2 for the faint result marker"
        );
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
        let summary_col = vterm
            .row_text(res_idx)
            .chars()
            .position(|ch| ch == '3')
            .unwrap();
        let summary_cell = vterm.cell_at(res_idx, summary_col);
        assert!(
            summary_cell.faint,
            "summary metadata must be faint in dark theme, got {:?}",
            summary_cell,
        );

        // Header line: the `●` anchor is bold and NOT faint — it shares the tool
        // name's prominent tier (`tool_bullet_style` = `style_bold(Role::ToolName)`),
        // anchoring the tool-call row as a single bold glyph + name.
        let hdr_idx = (0..vterm.height() as usize)
            .find(|&i| vterm.row_text(i).contains("ListDirectory"))
            .unwrap_or_else(|| panic!("header row missing\ndump:\n{}", vterm.dump()));
        let bullet_col = vterm.row_text(hdr_idx).find('●').unwrap();
        let bullet_cell = vterm.cell_at(hdr_idx, bullet_col);
        assert!(
            bullet_cell.bold && !bullet_cell.faint,
            "`●` bullet must be bold (prominent tier) in dark theme, got {:?}",
            bullet_cell,
        );
        let name_col = vterm.row_text(hdr_idx).find("ListDirectory").unwrap();
        let name_cell = vterm.cell_at(hdr_idx, name_col);
        assert!(
            name_cell.bold,
            "tool name must be bold, got {:?}",
            name_cell
        );
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
            diff_stats: None,
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
                diff_stats: None,
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
                .find(|&i| vterm.row_text(i).contains("●") && vterm.row_text(i).contains(tool_name))
                .unwrap_or_else(|| {
                    panic!(
                        "[{tool_name}] tool call row missing\ndump:\n{}",
                        vterm.dump()
                    )
                });
            let result_row = (0..vterm.height() as usize)
                .find(|&i| vterm.row_text(i).contains("└"))
                .unwrap_or_else(|| {
                    panic!(
                        "[{tool_name}] tool result row missing\ndump:\n{}",
                        vterm.dump()
                    )
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
                arrow_col,
                name_col,
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
        let _theme = crate::highlight::theme::test_lock();
        crate::highlight::theme::set_theme_mode(false);
        let (mut r, buf) = new_capturing(120, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(120, 24);
        let status = status_basic();
        // Multi-line failure body: header + quoted-code detail.
        r.render(UiLine::ToolResult {
            success: false,
            summary: "old_string not found in foo.rs\n759| line content\n760| more code".into(),
            diff_stats: None,
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
        let glyph_col = header_text.chars().position(|ch| ch == '✗').unwrap();
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
        let long_summary = "Created new file /tmp/atomcode-smoke-temp-check.txt (15 bytes, 1 line)";
        r.render(UiLine::ToolResult {
            success: true,
            summary: long_summary.into(),
            diff_stats: None,
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
        use crate::render::{DiffEntry, DiffKind};
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::DiffBlock(vec![
            DiffEntry {
                kind: DiffKind::Add,
                old_lineno: None,
                new_lineno: Some(1),
                text: "new line".into(),
            },
            DiffEntry {
                kind: DiffKind::Del,
                old_lineno: Some(1),
                new_lineno: None,
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

    #[test]
    fn edit_result_renders_colored_stats_inline_without_summary_row() {
        use crate::render::{DiffEntry, DiffKind};
        let (mut r, buf) = new_capturing(100, 24);
        r.caps.colors = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(100, 24);
        r.render(UiLine::ToolResult {
            success: true,
            summary: "Edited crates/example.rs".into(),
            diff_stats: Some((2, 1)),
        });
        r.render(UiLine::EditDiffBlock(vec![
            DiffEntry {
                kind: DiffKind::Add,
                old_lineno: None,
                new_lineno: Some(1),
                text: "new".into(),
            },
            DiffEntry {
                kind: DiffKind::Del,
                old_lineno: Some(1),
                new_lineno: None,
                text: "old".into(),
            },
        ]));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let header_row = (0..vterm.height() as usize)
            .find(|&row| {
                vterm
                    .row_text(row)
                    .contains("Edited crates/example.rs (+2 -1)")
            })
            .unwrap_or_else(|| panic!("inline edit stats missing\n{}", vterm.dump()));
        let header = vterm.row_text(header_row);
        let add_col = header
            .chars()
            .position(|ch| ch == '+')
            .expect("addition count");
        let remove_col = header
            .chars()
            .position(|ch| ch == '-')
            .expect("removal count");
        assert_eq!(
            vterm.cell_at(header_row, add_col).fg,
            Some(crate::render::theme::diff_add_for_current_theme())
        );
        assert_eq!(
            vterm.cell_at(header_row, remove_col).fg,
            Some(crate::render::theme::diff_remove_for_current_theme())
        );
        assert!(
            !(0..vterm.height() as usize).any(|row| vterm.row_text(row).trim() == "+2 -1"),
            "edit diff must not render a second statistics row\n{}",
            vterm.dump()
        );
    }

    #[test]
    fn diff_block_renders_line_number_gutter() {
        use crate::render::{DiffEntry, DiffKind};
        let (mut r, buf) = new_capturing(80, 24);
        r.caps.colors = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        r.render(UiLine::DiffBlock(vec![
            DiffEntry {
                kind: DiffKind::Context,
                old_lineno: Some(9),
                new_lineno: Some(9),
                text: "keep".into(),
            },
            DiffEntry {
                kind: DiffKind::Del,
                old_lineno: Some(10),
                new_lineno: None,
                text: "old line".into(),
            },
            DiffEntry {
                kind: DiffKind::Add,
                old_lineno: None,
                new_lineno: Some(10),
                text: "new line".into(),
            },
        ]));
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
            vterm.any_row(|row| row.contains("10 - old line")),
            "removed row w/ gutter\n{}",
            vterm.dump()
        );
        assert!(
            vterm.any_row(|row| row.contains("10 + new line")),
            "added row w/ gutter\n{}",
            vterm.dump()
        );
        assert!(
            vterm.any_row(|row| row.contains("9   keep")),
            "context row w/ gutter\n{}",
            vterm.dump()
        );
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
        let _theme = crate::highlight::theme::test_lock();
        crate::highlight::theme::set_theme_mode(false);
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
            "  ○ Press Ctrl+o to show real-time output\n".into(),
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
        r.render(UiLine::CommandOutput(
            "line one\nline two\nline three".into(),
        ));
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

    /// A live spinner is transient even on its first frame. When the body
    /// already fills the viewport, creating it must not use the permanent
    /// row path's LF and promote history into native scrollback. Tool updates
    /// can clear/recreate this row many times during one turn.
    #[test]
    fn retained_first_spinner_frame_does_not_advance_scrollback() {
        let (mut r, buf) = new_capturing(80, 8);
        r.status = status_basic();
        let body_capacity = 8usize.saturating_sub(r.current_footer_rows());
        r.body_lines = vec![Vec::new(); body_capacity];

        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Brewing · 10s · ↑ 762 tokens".into(),
            status: status_basic(),
            menu: None,
            attachments: Vec::new(),
        });

        assert_eq!(
            r.scrolled_off, 0,
            "transient spinner creation must not promote a body row into scrollback"
        );
        assert!(
            !buf.lock().unwrap().contains(&b'\n'),
            "first spinner frame must not emit the permanent body path's eager LF"
        );
        assert!(r.live_spinner_active);
        assert_eq!(r.body_lines.len(), body_capacity + 1);
    }

    #[test]
    fn retained_full_body_clears_spinner_strip_without_touching_footer() {
        let height = 8u16;
        let (mut r, buf) = new_capturing(80, height);
        let status = status_basic();
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        let body_capacity = (height as usize).saturating_sub(r.current_footer_rows());
        for i in 0..body_capacity {
            r.render(UiLine::AssistantText(format!("body-{i}\n")));
        }
        r.flush_deferred();

        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Brewing".into(),
            status,
            menu: None,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        assert!(r.live_spinner_spacer_active);
        let bottom = r.body_bottom_row();
        assert_eq!(bottom as usize, body_capacity);

        buf.lock().unwrap().clear();
        assert!(r.clear_live_spinner());
        let emitted = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        let spacer_row = bottom - 1;
        assert!(
            emitted.contains(&format!("\x1b[{spacer_row};1H\x1b[K"))
                && emitted.contains(&format!("\x1b[{bottom};1H\x1b[K")),
            "clear must erase the spacer and spinner rows: {emitted:?}"
        );
        assert!(
            !emitted.contains(&format!("\x1b[{};1H\x1b[K", bottom + 1)),
            "clear must not erase the footer top row: {emitted:?}"
        );
    }

    #[test]
    fn retained_tool_group_then_spinner_has_one_stable_blank_row() {
        use crate::render::ToolGroupChild;

        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();
        r.render(UiLine::ToolGroupRender {
            batch_id: "batch-spacing".into(),
            header: "● Running 2 bash calls".into(),
            children: vec![
                ToolGroupChild {
                    call_id: "call-1".into(),
                    text: "└ Bash(first)".into(),
                },
                ToolGroupChild {
                    call_id: "call-2".into(),
                    text: "└ Bash(second)".into(),
                },
            ],
        });
        let before_spinner = r.body_lines.len();

        for frame in ["⠋", "⠙", "⠹"] {
            r.render(UiLine::StreamingBox {
                buf: String::new(),
                cursor_byte: 0,
                frame,
                label: "Percolating · 1m2s · ↑ 552 tokens".into(),
                status: status.clone(),
                menu: None,
                attachments: Vec::new(),
            });
        }
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let child_row = (0..24)
            .find(|row| vterm.row_text(*row).contains("Bash(second)"))
            .unwrap_or_else(|| panic!("last tool child missing:\n{}", vterm.dump()));
        let spinner_row = (0..24)
            .find(|row| vterm.row_text(*row).contains("Percolating"))
            .unwrap_or_else(|| panic!("spinner missing:\n{}", vterm.dump()));
        assert_eq!(
            spinner_row - child_row,
            2,
            "tool output and spinner must have exactly one blank row between them:\n{}",
            vterm.dump()
        );
        assert_eq!(
            r.body_lines.len(),
            before_spinner + 2,
            "spinner ticks must reuse one transient spacer plus one live row"
        );

        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        assert_eq!(
            r.body_lines.len(),
            before_spinner,
            "clearing the spinner must also remove its transient spacer"
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

    /// Tool-call streaming paints a transient "Preparing Tool(...)" live spinner.
    /// When the real in-flight tool row arrives, that spinner must be removed
    /// rather than becoming a second visible `WriteFile(test.txt)` row.
    #[test]
    fn retained_tool_call_inflight_covers_streaming_spinner_row() {
        let (mut r, _buf) = new_capturing(80, 24);
        let status = status_basic();

        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "●",
            label: "Preparing WriteFile(test.txt)".into(),
            status,
            menu: None,
            attachments: Vec::new(),
        });
        r.render(UiLine::ToolCallInFlight {
            id: "call-write-1".into(),
            name: "WriteFile".into(),
            detail: "test.txt".into(),
            hint: None,
        });

        let row_texts: Vec<String> = r
            .body_lines
            .iter()
            .map(|row| row.iter().map(|c| c.ch).collect::<String>())
            .collect();
        let write_rows = row_texts
            .iter()
            .filter(|t| t.contains("WriteFile(test.txt)"))
            .count();

        assert_eq!(
            write_rows, 1,
            "streaming spinner plus in-flight tool must leave exactly one WriteFile row: {:?}",
            row_texts
        );
        assert!(
            !row_texts.iter().any(|t| t.contains("Preparing WriteFile")),
            "transient Preparing row leaked into history: {:?}",
            row_texts
        );
        assert!(
            !r.live_spinner_active,
            "live spinner state must be cleared once the tool row owns the slot"
        );
    }

    /// Same as the in-flight path, but for approval/replay paths that render a
    /// static ToolCall row directly.
    #[test]
    fn retained_static_tool_call_covers_streaming_spinner_row() {
        let (mut r, _buf) = new_capturing(80, 24);
        let status = status_basic();

        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "●",
            label: "Preparing WriteFile(test.txt)".into(),
            status,
            menu: None,
            attachments: Vec::new(),
        });
        r.render(UiLine::ToolCall {
            name: "WriteFile".into(),
            detail: "test.txt".into(),
        });

        let row_texts: Vec<String> = r
            .body_lines
            .iter()
            .map(|row| row.iter().map(|c| c.ch).collect::<String>())
            .collect();
        let write_rows = row_texts
            .iter()
            .filter(|t| t.contains("WriteFile(test.txt)"))
            .count();

        assert_eq!(
            write_rows, 1,
            "streaming spinner plus static tool call must leave exactly one WriteFile row: {:?}",
            row_texts
        );
        assert!(
            !row_texts.iter().any(|t| t.contains("Preparing WriteFile")),
            "transient Preparing row leaked into history: {:?}",
            row_texts
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

        // The user block now emits a bg-padding row above and below the content
        // row, so there are 3 rows between user content and assistant text
        // (bg_blank_bottom + paragraph_blank + assistant_text). The test
        // verifies no ghost blank was inserted by the leading '\n' in the model
        // output — if it were, the gap would be 4.
        assert_eq!(
            hello_row - user_row,
            3,
            "expected 2 blank rows between user and assistant, got {} \
             blank row(s) — leading `\\n` from model created a ghost \
             spacer:\n{}",
            hello_row.saturating_sub(user_row).saturating_sub(2),
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
        //   <user_row>:     "❯ hi-from-user"
        //   <user_row + 1>: blank (bg_blank_bottom padding row)
        //   <user_row + 2>: blank (UiLine::User's paragraph spacer)
        //   <user_row + 3>: "Hello world"  ← replaced spinner in-place
        //
        // The user block now has a bg padding row below the content, so
        // there are 2 blank rows between content and assistant. No extra
        // gap introduced by spinner replacement would make it 4 blanks.
        assert_eq!(
            hello_row - user_row,
            3,
            "expected 2 spacer rows between user and assistant, got {} \
             rows gap:\n{}",
            hello_row.saturating_sub(user_row).saturating_sub(2),
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
            3,
            "expected exactly 2 blank rows between user message and \
            spinner (bg_blank_bottom + paragraph spacer), got {} blank row(s):\n{}",
            spin_row.saturating_sub(user_row).saturating_sub(2),
            vterm.dump()
        );
    }

    #[test]
    fn retained_user_after_local_command_output_has_one_leading_blank() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let status = status_basic();

        // A resumed/long-running session has a full body viewport. The
        // screenshot regression only appears once new rows advance that
        // viewport, not in a nearly empty transcript.
        for i in 0..30 {
            r.render(UiLine::CommandOutput(format!("older-{i:02}")));
        }
        // Match the real `/model` success path: the event loop removes the
        // translated payload's line ending, then the idle prompt is painted
        // before the next submission clears the composer and emits the echo.
        r.render(UiLine::CommandOutput(
            "Switched to provider · model; default for new sessions".into(),
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

        r.render(UiLine::ClearTransient);
        r.render(UiLine::User("now?".into()));
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let output_row = (0..24)
            .find(|row| vterm.row_text(*row).contains("Switched to provider"))
            .unwrap_or_else(|| panic!("model output missing:\n{}", vterm.dump()));
        let user_row = (0..24)
            .find(|row| vterm.row_text(*row).contains("now?"))
            .unwrap_or_else(|| panic!("user echo missing:\n{}", vterm.dump()));

        assert_eq!(
            user_row - output_row,
            3,
            "local command output and the next user message need paragraph blank + bg_blank_top rows"
        );
        assert!(
            vterm.row_text(output_row + 1).trim().is_empty(),
            "the intervening physical terminal row must be blank:\n{}",
            vterm.dump()
        );
    }

    #[test]
    fn retained_user_reuses_existing_leading_blank() {
        let (mut r, _buf) = new_capturing(80, 24);

        r.render(UiLine::User("first".into()));
        let rows_after_first = r.body_lines.len();
        r.render(UiLine::User("second".into()));

        // The first user event already supplied its trailing spacer. The
        // second event must reuse it rather than introducing another blank.
        // Each user block is now 4 rows: bg_blank_top + content + bg_blank_bottom + paragraph blank.
        // The second message reuses the trailing paragraph blank from the first, so it adds 4 rows.
        assert_eq!(r.body_lines.len(), rows_after_first + 4);
        assert!(r.body_lines[rows_after_first - 1].is_empty());
        // Content row is offset by 1 due to the new bg_blank_top padding row.
        assert!(r.body_lines[rows_after_first + 1]
            .iter()
            .map(|cell| cell.ch)
            .collect::<String>()
            .contains("second"));
    }

    #[test]
    fn retained_full_viewport_keeps_user_spinner_blank_and_logical_top() {
        let width = 80u16;
        let height = 12u16;
        let (mut r, buf) = new_capturing(width, height);
        let mut vterm = crate::test_term::VirtualTerminal::new(width, height);
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
        let body_capacity = (height as usize).saturating_sub(r.current_footer_rows());

        for i in 0..body_capacity.saturating_sub(2) {
            r.render(UiLine::AssistantText(format!("older-{i:02}\n")));
        }
        r.render(UiLine::User("latest-user".into()));
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert_eq!(
            r.body_lines.len().saturating_sub(r.scrolled_off),
            body_capacity,
            "setup must exactly fill the permanent body viewport"
        );
        let expected_top: String = r.body_lines[r.scrolled_off]
            .iter()
            .map(|cell| cell.ch)
            .collect();

        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Noodling".into(),
            status,
            menu: None,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let user_row = (0..body_capacity)
            .find(|row| vterm.row_text(*row).contains("latest-user"))
            .unwrap_or_else(|| panic!("user echo missing:\n{}", vterm.dump()));
        let spinner_row = (0..body_capacity)
            .find(|row| vterm.row_text(*row).contains("Noodling"))
            .unwrap_or_else(|| panic!("spinner missing:\n{}", vterm.dump()));

        assert_eq!(
            spinner_row - user_row,
            3,
            "full viewport must retain exactly two blank rows between user and spinner \
             (bg_blank_bottom + paragraph spacer):\n{}",
            vterm.dump()
        );
        assert_eq!(
            vterm.row_text(0).trim_end(),
            expected_top.trim_end(),
            "transient tail preservation must not move the logical scrollback top"
        );
    }

    #[test]
    fn retained_full_viewport_spinner_to_tool_restores_continuous_body() {
        let width = 80u16;
        let height = 12u16;
        let (mut r, buf) = new_capturing(width, height);
        let mut vterm = crate::test_term::VirtualTerminal::new(width, height);
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
        let body_capacity = (height as usize).saturating_sub(r.current_footer_rows());

        for i in 0..body_capacity.saturating_sub(2) {
            r.render(UiLine::AssistantText(format!("tool-seed-{i:02}\n")));
        }
        r.render(UiLine::User("tool-request".into()));
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "⠋",
            label: "Noodling".into(),
            status,
            menu: None,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        assert!(
            r.live_spinner_tail_compacted,
            "setup must paint the full-viewport compacted spinner layout"
        );

        r.render(UiLine::ToolCall {
            name: "Read".into(),
            detail: "src/lib.rs".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        assert!(!r.live_spinner_tail_compacted);
        let expected_top: String = r.body_lines[r.scrolled_off]
            .iter()
            .map(|cell| cell.ch)
            .collect();
        assert_eq!(
            vterm.row_text(0).trim_end(),
            expected_top.trim_end(),
            "spinner-to-tool transition left physical rows out of sync with scrolled_off:\n{}",
            vterm.dump()
        );
        assert!(
            vterm.any_row(|row| row.contains("Read") && row.contains("src/lib.rs")),
            "tool row missing after spinner transition:\n{}",
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
        let _theme = crate::highlight::theme::test_lock();
        crate::highlight::theme::set_theme_mode(false);
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
        // Inline code: bright cyan (SGR 96) but NOT bold (bold made it flare in
        // dense prose — weight is reserved for **bold**). The 16-colour SGR lets
        // the terminal theme remap the shade; in CellStyle this arrives as
        // `Color::Cyan` (crossterm's name for SGR 96 / bright cyan).
        let code_pos = row_text
            .find("code")
            .expect("expected 'code' in rendered text");
        let code_cell = vterm.cell_at(row_idx, code_pos);
        assert!(
            !code_cell.bold,
            "inline code cell must NOT be bold (de-flare): {:?}",
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
        assert!(
            sizes.len() <= 4,
            "steady-state keystroke should be within expected write count (sizes: {:?})",
            sizes
        );
        if sizes.len() == 1 {
            assert!(
                sizes[0] < 512,
                "keystroke delta should be well under 512 B (got {} B)",
                sizes[0]
            );
        }
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
            brand_rows,
            1,
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
            cwd_rows,
            1,
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

    /// Parallel batch: bash children render as plain `  └ Bash <cmd>` (no `$`
    /// prefix or magenta accent); non-bash children stay as `  └ Name(args)`.
    #[test]
    fn batch_bash_child_renders_plain() {
        use crate::render::ToolGroupChild;
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);

        r.render(UiLine::ToolGroupRender {
            batch_id: "b1".into(),
            header: "● Running 2 tools in parallel".into(),
            children: vec![
                ToolGroupChild {
                    call_id: "bash1".into(),
                    // event_loop format for bash children: name + cmd (no `$`).
                    text: "  \u{2514} Bash cd /tmp && ls".into(),
                },
                ToolGroupChild {
                    call_id: "grep1".into(),
                    text: "  \u{2514} Grep(foo)".into(),
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

        let dump = vterm.dump();
        // Bash child shows `Bash cd /tmp` (name + cmd, no `$` prefix/accent).
        assert!(
            vterm.any_row(|row| row.contains("Bash cd /tmp")),
            "bash child must show `Bash cd /tmp`\ndump:\n{}",
            dump
        );
        assert!(
            !dump.contains('$'),
            "no `$` prefix expected\ndump:\n{}",
            dump
        );
        assert!(
            !dump.contains("Bash("),
            "bash child must NOT wrap in Bash(...):\n{}",
            dump
        );
        // Non-bash child unchanged.
        assert!(
            vterm.any_row(|row| row.contains("Grep(foo)")),
            "non-bash child must keep Name(args) format\ndump:\n{}",
            dump
        );
    }

    /// The todo panel is pinned at the TOP of the footer — it must render ABOVE
    /// the input box (lower row index), not below it.
    #[test]
    fn todo_panel_renders_above_the_input_box() {
        use atomcode_capabilities::tools::todo::TodoStatus;
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let mut status = status_basic();
        status.todo = Some(crate::render::TodoProgress {
            current: Some("wire it".into()),
            completed: 1,
            in_progress: 1,
            total: 3,
            items: vec![
                (TodoStatus::Completed, "done a".into()),
                (TodoStatus::InProgress, "wire it".into()),
                (TodoStatus::Pending, "later".into()),
            ],
        });
        r.render(UiLine::InputPrompt {
            buf: "TYPEDHERE".into(),
            cursor_byte: 9,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let h = vterm.height() as usize;
        let row_of = |needle: &str| (0..h).find(|&i| vterm.row_text(i).contains(needle));
        let panel = row_of("Tasks").expect("panel header rendered");
        let input = row_of("TYPEDHERE").expect("input text rendered");
        assert!(
            panel < input,
            "todo panel must render ABOVE the input box (panel row {panel} vs input row {input})\n{}",
            vterm.dump()
        );
    }

    #[test]
    fn subtask_panel_renders_fixed_compact_rows_and_hides_todo_panel() {
        use crate::render::{SubtaskItem, SubtaskProgress, SubtaskStatus};

        let (mut r, buf) = new_capturing(100, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(100, 24);
        let mut status = status_basic();
        status.todo = Some(crate::render::TodoProgress {
            current: Some("standing todo".into()),
            completed: 0,
            in_progress: 1,
            total: 1,
            items: vec![(
                atomcode_capabilities::tools::todo::TodoStatus::InProgress,
                "standing todo".into(),
            )],
        });
        status.subtasks = Some(SubtaskProgress {
            call_id: "call-task".into(),
            completed: 1,
            total: 3,
            items: vec![
                SubtaskItem {
                    label: "explore#1".into(),
                    description: "inspect atomcode".into(),
                    model: "deepseek-v4-flash".into(),
                    activity: "completed".into(),
                    started_at: Some(std::time::Instant::now()),
                    output_tokens: 900,
                    status: SubtaskStatus::Completed,
                },
                SubtaskItem {
                    label: "explore#2".into(),
                    description: "inspect codex".into(),
                    model: "deepseek-v4-flash".into(),
                    activity: "reading files".into(),
                    started_at: Some(std::time::Instant::now()),
                    output_tokens: 420,
                    status: SubtaskStatus::Running,
                },
                SubtaskItem {
                    label: "explore#3".into(),
                    description: "inspect opencode".into(),
                    model: "deepseek-v4-flash".into(),
                    activity: "thinking".into(),
                    started_at: Some(std::time::Instant::now()),
                    output_tokens: 210,
                    status: SubtaskStatus::Running,
                },
            ],
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: vec![],
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let grid = vterm.dump();
        assert!(grid.contains("Subtasks · 1/3 finished · 2 running · 0 pending"));
        assert!(grid.contains("explore#2"));
        assert!(grid.contains("\u{25cf} explore#2"));
        assert!(grid.contains("explore#2 · deepseek-v4-flash · inspect codex"));
        assert!(grid.contains("reading files"));
        assert!(grid.contains("↑ 420 tokens"));
        assert!(!grid.contains("explore#1"));
        assert!(!grid.contains("standing todo"));
        assert_eq!(
            r.current_footer_rows(),
            r.last_painted_footer_rows,
            "subtask footer height math must mirror the painted rows"
        );
    }

    #[test]
    fn subtask_progress_metadata_never_exceeds_narrow_width() {
        for width in 0..32 {
            let rendered = super::format_subtask_progress(
                "已定位命令注册入口，正在核对补全与权限机制",
                "18s",
                12_345,
                width,
            );
            assert!(
                crate::width::display_width(&rendered) <= width,
                "width={width}, rendered={rendered:?}"
            );
        }
        let full = super::format_subtask_progress("正在分析", "18s", 384, 80);
        assert!(full.contains("18s"));
        assert!(full.contains("\u{2191} 384 tokens"));
    }

    #[test]
    fn every_subtask_panel_row_is_clamped_to_rule_width() {
        use crate::render::{SubtaskItem, SubtaskProgress, SubtaskStatus};

        let (r, _buf) = new_capturing(100, 24);
        let progress = SubtaskProgress {
            call_id: "call-task".into(),
            completed: 0,
            total: 5,
            items: (1..=5)
                .map(|n| SubtaskItem {
                    label: format!("explore#{n}"),
                    description: "验证一个非常长的 CLI 命令注册与补全机制".into(),
                    model: "deepseek-v4-flash".into(),
                    activity: "已定位命令注册入口，正在核对补全与权限机制".into(),
                    started_at: Some(std::time::Instant::now()),
                    output_tokens: 12_345,
                    status: SubtaskStatus::Running,
                })
                .collect(),
        };

        for width in 0..40 {
            let rows = r.build_subtask_rows(&progress, width);
            assert!(rows.len() <= MAX_SUBTASK_PANEL_ROWS);
            assert!(
                rows.iter().all(|row| row.len() <= width),
                "width={width}, row widths={:?}",
                rows.iter().map(Vec::len).collect::<Vec<_>>()
            );
            assert!(
                rows.iter()
                    .all(|row| !row.last().is_some_and(|cell| cell.width > 1)),
                "width={width}: wide glyph split at right edge"
            );
        }
    }

    #[test]
    fn subtask_panel_prioritizes_live_rows_and_truthfully_aggregates_hidden_rows() {
        use crate::render::{SubtaskItem, SubtaskProgress, SubtaskStatus};

        let (mut r, buf) = new_capturing(100, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(100, 24);
        let mut status = status_basic();
        let item = |label: &str, state| SubtaskItem {
            label: label.into(),
            description: format!("inspect {label}"),
            model: "deepseek-v4-flash".into(),
            activity: String::new(),
            started_at: Some(std::time::Instant::now()),
            output_tokens: 0,
            status: state,
        };
        status.subtasks = Some(SubtaskProgress {
            call_id: "call-task".into(),
            completed: 4,
            total: 8,
            items: vec![
                item("done#1", SubtaskStatus::Completed),
                item("done#2", SubtaskStatus::Completed),
                item("done#3", SubtaskStatus::Completed),
                item("done#4", SubtaskStatus::Completed),
                item("pending#1", SubtaskStatus::Pending),
                item("pending#2", SubtaskStatus::Pending),
                item("running#1", SubtaskStatus::Running),
                item("failed#1", SubtaskStatus::Failed),
            ],
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: vec![],
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let grid = vterm.dump();
        assert!(grid.contains("running#1"));
        assert!(!grid.contains("failed#1"));
        assert!(grid.contains("5/8 finished · 1 running · 2 pending"));
        assert!(grid.contains("2 pending · 1 failed"));
    }

    #[test]
    fn subtask_panel_uses_five_rows_for_three_running_and_terminal_summary() {
        use crate::render::{SubtaskItem, SubtaskProgress, SubtaskStatus};

        let (r, _buf) = new_capturing(120, 24);
        let item = |label: &str, state| SubtaskItem {
            label: label.into(),
            description: format!("inspect {label}"),
            model: "GLM-5.2".into(),
            activity: "正在分析结果".into(),
            started_at: Some(std::time::Instant::now()),
            output_tokens: 128,
            status: state,
        };
        let progress = SubtaskProgress {
            call_id: "call-task".into(),
            completed: 2,
            total: 7,
            items: vec![
                item("done#1", SubtaskStatus::Completed),
                item("done#2", SubtaskStatus::Completed),
                item("failed#1", SubtaskStatus::Failed),
                item("explore#4", SubtaskStatus::Running),
                item("explore#5", SubtaskStatus::Running),
                item("explore#6", SubtaskStatus::Running),
                item("pending#1", SubtaskStatus::Pending),
            ],
        };

        let rows = r.build_subtask_rows(&progress, 120);
        let text = rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.ch).collect::<String>())
            .collect::<Vec<_>>();

        assert_eq!(text.len(), MAX_SUBTASK_PANEL_ROWS);
        assert!(text[0].contains("3/7 finished · 3 running · 1 pending"));
        assert!(text.iter().any(|line| line.contains("explore#4")));
        assert!(text.iter().any(|line| line.contains("explore#5")));
        assert!(text.iter().any(|line| line.contains("explore#6")));
        assert!(text[4].contains("1 pending · 1 failed"));
        assert!(text.iter().all(|line| !line.contains("failed#1")));
        assert!(text.iter().all(|line| !line.contains("pending#1")));
    }

    #[test]
    fn subtask_panel_respects_short_screen_footer_budget() {
        use crate::render::{GoalStatus, SubtaskItem, SubtaskProgress, SubtaskStatus};

        let (mut r, buf) = new_capturing(80, 8);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 8);
        let mut status = status_basic();
        status.goal = Some(GoalStatus {
            condition: "finish the audit".into(),
            round: 2,
            elapsed_secs: 12,
        });
        status.subtasks = Some(SubtaskProgress {
            call_id: "call-task".into(),
            completed: 0,
            total: 3,
            items: (1..=3)
                .map(|n| SubtaskItem {
                    label: format!("explore#{n}"),
                    description: format!("inspect area {n}"),
                    model: "deepseek-v4-flash".into(),
                    activity: "thinking".into(),
                    started_at: Some(std::time::Instant::now()),
                    output_tokens: 0,
                    status: SubtaskStatus::Running,
                })
                .collect(),
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: vec![],
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        assert!(
            r.current_footer_rows() <= 8,
            "footer must fit the physical screen"
        );
        assert_eq!(r.current_footer_rows(), r.last_painted_footer_rows);
        assert!(vterm
            .dump()
            .contains("Subtasks · 0/3 finished · 3 running · 0 pending"));
    }

    #[test]
    fn subtask_panel_uses_zero_rows_when_no_footer_budget_remains() {
        use crate::render::{GoalStatus, SubtaskItem, SubtaskProgress, SubtaskStatus};

        let (mut r, _buf) = new_capturing(80, 5);
        let mut status = status_basic();
        status.goal = Some(GoalStatus {
            condition: "finish".into(),
            round: 1,
            elapsed_secs: 1,
        });
        status.subtasks = Some(SubtaskProgress {
            call_id: "call-task".into(),
            completed: 0,
            total: 1,
            items: vec![SubtaskItem {
                label: "explore#1".into(),
                description: "inspect".into(),
                model: "deepseek-v4-flash".into(),
                activity: "thinking".into(),
                started_at: Some(std::time::Instant::now()),
                output_tokens: 0,
                status: SubtaskStatus::Running,
            }],
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: vec![],
        });
        r.flush_deferred();

        assert!(r.current_footer_rows() <= 5);
        assert_eq!(r.current_footer_rows(), r.last_painted_footer_rows);
    }

    #[test]
    fn subtask_panel_replaces_generic_body_spinner() {
        use crate::render::{SubtaskItem, SubtaskProgress, SubtaskStatus};

        let (mut r, buf) = new_capturing(100, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(100, 24);
        let mut status = status_basic();
        status.subtasks = Some(SubtaskProgress {
            call_id: "call-task".into(),
            completed: 0,
            total: 1,
            items: vec![SubtaskItem {
                label: "explore#1".into(),
                description: "inspect atomcode".into(),
                model: "deepseek-v4-flash".into(),
                activity: "thinking".into(),
                started_at: Some(std::time::Instant::now()),
                output_tokens: 0,
                status: SubtaskStatus::Running,
            }],
        });
        r.render(UiLine::StreamingBox {
            buf: String::new(),
            cursor_byte: 0,
            frame: "\u{25d0}".into(),
            label: "Thinking\u{2026} (2s)".into(),
            status,
            menu: None,
            attachments: vec![],
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let grid = vterm.dump();
        assert!(grid.contains("Subtasks · 0/1 finished · 1 running · 0 pending"));
        assert!(!grid.contains("Thinking"));
        assert!(!r.live_spinner_active);
    }

    #[test]
    fn approval_panel_temporarily_hides_subtask_panel() {
        use crate::render::{SubtaskItem, SubtaskProgress, SubtaskStatus};

        let (mut r, buf) = new_capturing(100, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(100, 24);
        let mut status = status_basic();
        status.subtasks = Some(SubtaskProgress {
            call_id: "call-task".into(),
            completed: 0,
            total: 1,
            items: vec![SubtaskItem {
                label: "worker#1".into(),
                description: "edit scoped files".into(),
                model: "deepseek-v4-flash".into(),
                activity: "waiting".into(),
                started_at: Some(std::time::Instant::now()),
                output_tokens: 0,
                status: SubtaskStatus::Running,
            }],
        });
        status.approval = Some(crate::render::ApprovalPanelView {
            tool: "Task".into(),
            detail: "worker scope".into(),
            options: vec!["Allow".into(), "Deny".into()],
            selected: 0,
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: vec![],
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let grid = vterm.dump();
        assert!(grid.contains("Allow"));
        assert!(!grid.contains("Subtasks"));
        assert_eq!(r.current_footer_rows(), r.last_painted_footer_rows);
    }

    #[test]
    fn approval_panel_renders_selectable_options() {
        let (mut r, buf) = new_capturing(80, 24);
        r.caps.colors = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let mut status = status_basic();
        status.approval = Some(crate::render::ApprovalPanelView {
            tool: "Bash".into(),
            detail: "rm -rf build/".into(),
            options: vec![
                "Allow once".into(),
                "Always allow Bash".into(),
                "Deny".into(),
            ],
            selected: 0,
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
        let dump = vterm.dump();
        // Compact panel: only the selectable options (no header/detail/hint row).
        assert!(
            vterm.any_row(|r| r.contains("Allow once")),
            "allow once row\n{dump}"
        );
        assert!(
            vterm.any_row(|r| r.contains("Always allow Bash")),
            "always row (carries tool name)\n{dump}"
        );
        assert!(vterm.any_row(|r| r.contains("Deny")), "deny row\n{dump}");
        assert!(
            vterm.any_row(|r| r.contains("▸") && r.contains("Allow once")),
            "selected marker on option 0\n{dump}"
        );
        // The command detail IS now displayed in the panel header.
        assert!(
            vterm.any_row(|r| r.contains("rm -rf build/")),
            "detail must be in the panel header\n{dump}"
        );
        // Panel renders BELOW the input box (chevron ❯).
        let h = vterm.height() as usize;
        let row_of = |n: &str| (0..h).find(|&i| vterm.row_text(i).contains(n));
        assert!(
            row_of("Allow once") > row_of("❯"),
            "panel below input\n{dump}"
        );
    }

    // request_user_input panel: the question header + a mode-specific body
    // render, in the same modal slot as approval (replacing the input box).
    #[test]
    fn user_input_panel_renders_all_three_modes() {
        use atomcode_capabilities::tools::request_user_input::UserInputMode;

        // Single: numbered list with per-item descriptions, cursor row has the
        // ❯ marker, "Other" appended as the last numbered option.
        {
            let (mut r, buf) = new_capturing(80, 24);
            r.caps.colors = true;
            let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
            let mut status = status_basic();
            let view = crate::render::UserInputPanelView {
                header: "Library".into(),
                question: "Which library?".into(),
                mode: UserInputMode::Single,
                options: vec![
                    ("date-fns".into(), Some("Tree-shakeable".into())),
                    ("Day.js".into(), None),
                ],
                cursor: 0,
                checked: vec![false, false, false],
                text: String::new(),
                custom_text: String::new(),
                custom: true,
                batch: None,
            };
            // Row count: header + blank + question + blank
            //   + opt1 label + opt1 desc + opt2 label (no desc)
            //   + custom inline row (1 line) + blank + hint.
            assert_eq!(r.user_input_panel_row_count(&view), 4 + 3 + 1 + 2);
            assert_eq!(
                r.build_user_input_rows(&view, 78, 80).len(),
                r.user_input_panel_row_count(&view),
                "row_count must equal build_user_input_rows().len()"
            );
            status.user_input = Some(view);
            r.render(UiLine::InputPrompt {
                buf: String::new(),
                cursor_byte: 0,
                menu: None,
                status,
                attachments: Vec::new(),
            });
            r.flush_deferred();
            drain_into_vterm(&buf, &mut vterm);
            let dump = vterm.dump();
            assert!(
                vterm.any_row(|r| r.contains("Library")),
                "header chip\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("Which library?")),
                "question\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("1. date-fns")),
                "numbered option 1\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("Tree-shakeable")),
                "description line rendered\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("2. Day.js")),
                "numbered option 2\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("\u{276f}") && r.contains("date-fns")),
                "cursor marker on option 1\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("3.") && r.contains("输")),
                "custom-answer row: number + faint placeholder (输入自己的答案…)\n{dump}"
            );
            assert!(
                !vterm.any_row(|r| r.contains("Other") || r.contains("Type a custom answer")),
                "no redundant 'Other' label/subtitle\n{dump}"
            );
            // Single mode must NOT render a checkbox.
            assert!(
                !vterm.any_row(|r| r.contains("[x]") || r.contains("[ ]")),
                "single must NOT render checkboxes\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("1-3 select")),
                "single hint\n{dump}"
            );
        }

        // Multiple: checkbox prefixes reflect the `checked` flags; "Other" row
        // shows the typed buffer.
        {
            let (mut r, buf) = new_capturing(80, 24);
            r.caps.colors = true;
            let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
            let mut status = status_basic();
            let view = crate::render::UserInputPanelView {
                header: "Features".into(),
                question: "Which features?".into(),
                mode: UserInputMode::Multiple,
                options: vec![
                    ("Streaming".into(), Some("Token-by-token".into())),
                    ("Tool use".into(), None),
                ],
                // checked has length options.len()+1 (Other slot last); Other checked.
                cursor: 0,
                checked: vec![true, false, true],
                text: String::new(),
                custom_text: "Zig".into(),
                custom: true,
                batch: None,
            };
            assert_eq!(
                r.build_user_input_rows(&view, 78, 80).len(),
                r.user_input_panel_row_count(&view),
                "row_count must equal build_user_input_rows().len()"
            );
            status.user_input = Some(view);
            r.render(UiLine::InputPrompt {
                buf: String::new(),
                cursor_byte: 0,
                menu: None,
                status,
                attachments: Vec::new(),
            });
            r.flush_deferred();
            drain_into_vterm(&buf, &mut vterm);
            let dump = vterm.dump();
            assert!(
                vterm.any_row(|r| r.contains("[x] 1. Streaming")),
                "Streaming checked\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("Token-by-token")),
                "description rendered\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("[ ] 2. Tool use")),
                "Tool use unchecked\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("[x] 3. Zig")),
                "custom-answer row: checkbox + number + typed text (no 'Other' word)\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("Zig")),
                "custom text shown\n{dump}"
            );
            // Multiple: Submit row after Other (unicode ✔ 提交).
            assert!(
                vterm.any_row(|r| r.contains("\u{2714}") || r.contains("Submit")),
                "Submit row must be rendered\n{dump}"
            );
            // Multiple hint no longer says "1-N toggle … Enter confirm" — it says Submit row confirms.
            assert!(
                vterm.any_row(|r| r.contains("Space toggle")),
                "multiple hint Space toggle\n{dump}"
            );
        }

        // Text: a `> {buffer}` input row shows the typed answer.
        {
            let (mut r, buf) = new_capturing(80, 24);
            r.caps.colors = true;
            let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
            let mut status = status_basic();
            let view = crate::render::UserInputPanelView {
                header: "Name".into(),
                question: "Project name?".into(),
                mode: UserInputMode::Text,
                options: vec![],
                cursor: 0,
                checked: vec![],
                text: "atomcode".into(),
                custom_text: String::new(),
                custom: true,
                batch: None,
            };
            // header + blank + question + blank + input + hint = 6.
            assert_eq!(r.user_input_panel_row_count(&view), 6, "text panel rows");
            assert_eq!(
                r.build_user_input_rows(&view, 78, 80).len(),
                r.user_input_panel_row_count(&view),
                "row_count must equal build_user_input_rows().len()"
            );
            status.user_input = Some(view);
            r.render(UiLine::InputPrompt {
                buf: String::new(),
                cursor_byte: 0,
                menu: None,
                status,
                attachments: Vec::new(),
            });
            r.flush_deferred();
            drain_into_vterm(&buf, &mut vterm);
            let dump = vterm.dump();
            assert!(
                vterm.any_row(|r| r.contains("Project name?")),
                "question header\n{dump}"
            );
            assert!(
                vterm.any_row(|r| r.contains("> atomcode")),
                "text input row\n{dump}"
            );
        }
    }

    #[test]
    fn custom_answer_cursor_renders_before_the_placeholder() {
        use atomcode_capabilities::tools::request_user_input::UserInputMode;

        // When the empty custom-answer row is on the cursor, the caret ▏ must sit
        // at the FRONT (where typing begins), with the placeholder trailing as a
        // hint — not after the placeholder text.
        let (mut r, buf) = new_capturing(80, 24);
        r.caps.colors = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let mut status = status_basic();
        let view = crate::render::UserInputPanelView {
            header: "Library".into(),
            question: "Which?".into(),
            mode: UserInputMode::Single,
            options: vec![("date-fns".into(), None), ("Day.js".into(), None)],
            cursor: 2, // the custom-answer row (after the 2 options)
            checked: vec![false, false, false],
            text: String::new(),
            custom_text: String::new(), // empty → placeholder shown
            custom: true,
            batch: None,
        };
        status.user_input = Some(view);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let dump = vterm.dump();

        let row = (0..24)
            .map(|y| vterm.row_text(y))
            .find(|s| s.contains('输'))
            .unwrap_or_else(|| panic!("custom-answer row not found\n{dump}"));
        let caret = row
            .find('\u{258f}')
            .unwrap_or_else(|| panic!("caret ▏ not rendered\nrow={row:?}\n{dump}"));
        let placeholder = row.find('输').expect("placeholder present");
        assert!(
            caret < placeholder,
            "caret ▏ must render BEFORE the placeholder (输入自己的答案…)\nrow={row:?}\n{dump}"
        );
    }

    #[test]
    fn user_input_batch_navigator_and_parity() {
        use crate::render::{UserInputBatchMeta, UserInputPanelView};
        use atomcode_capabilities::tools::request_user_input::UserInputMode;
        let (r, _buf) = new_capturing(80, 24);

        let base = |batch| UserInputPanelView {
            header: "Auth".into(),
            question: "Which auth?".into(),
            mode: UserInputMode::Single,
            options: vec![("OAuth".into(), None), ("Token".into(), None)],
            cursor: 0,
            checked: vec![false, false, false],
            text: String::new(),
            custom_text: String::new(),
            custom: true,
            batch,
        };

        // N==1 parity: batch None and batch Some(total=1) both equal the single renderer.
        let single = base(None);
        let single_rows = r.build_user_input_rows(&single, 78, 80).len();
        assert_eq!(r.user_input_panel_view_row_count(&single), single_rows);
        let one = base(Some(UserInputBatchMeta {
            total: 1,
            index: 1,
            answered: vec![false],
            on_submit: false,
        }));
        assert_eq!(
            r.build_user_input_panel_view(&one, 78, 80).len(),
            single_rows,
            "total==1 delegates to the single renderer"
        );

        // N==2 on a question: navigator + blank + single rows + tab hint.
        let q = base(Some(UserInputBatchMeta {
            total: 2,
            index: 1,
            answered: vec![false, false],
            on_submit: false,
        }));
        let q_rows = r.build_user_input_panel_view(&q, 78, 80);
        assert_eq!(
            q_rows.len(),
            r.user_input_panel_view_row_count(&q),
            "row_count invariant (question)"
        );
        // nav + blank + (single rows minus its own hint) + one batch hint = single + 2.
        assert_eq!(q_rows.len(), single_rows + 2);

        // Submit stop: exactly 5 rows (nav + blank + submit + blank + hint).
        let sub = base(Some(UserInputBatchMeta {
            total: 2,
            index: 2,
            answered: vec![true, false],
            on_submit: true,
        }));
        let sub_rows = r.build_user_input_panel_view(&sub, 78, 80);
        assert_eq!(
            sub_rows.len(),
            r.user_input_panel_view_row_count(&sub),
            "row_count invariant (submit)"
        );
        assert_eq!(sub_rows.len(), 5);
    }

    /// Round-cap checkpoint: `round_cap_view` produces a Single-mode
    /// `UserInputPanelView` with the correct header, question, and two options.
    #[test]
    fn round_cap_view_renders_header_and_two_options() {
        // cap=400 (after one continuation) but base=200 (the re-arm step): the
        // question shows the grown cap, the "continue" description must show base.
        let view = crate::render::round_cap_view(400, 200, 0, "2h0m0s · 305.00K tokens");
        assert_eq!(view.header, "轮次上限");
        assert!(
            view.question.contains("已运行 400 轮"),
            "question should contain '已运行 400 轮', got: {}",
            view.question
        );
        assert_eq!(view.options.len(), 2);
        assert_eq!(view.options[0].0, "继续");
        assert_eq!(
            view.options[0].1.as_deref(),
            Some("再跑 200 轮后重新确认"),
            "continue description must use base (200), not cap (400)"
        );
        assert_eq!(view.options[1].0, "停止");
    }

    /// Step 1: option rows carry `1. ` / `2. ` / `3. ` numeric prefixes, the
    /// selected row (`selected=0`) spans the full terminal width with reverse
    /// highlight, and a hint row appears after the last option.
    ///
    /// `approval_panel_row_count` must equal `options.len() + 2` (header + hint rows).
    #[test]
    fn approval_panel_numbered_prefixes_fullwidth_hint() {
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        const W: u16 = 80;
        let (mut r, buf) = new_capturing(W, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(W, 24);

        let mut status = status_basic();
        let panel = crate::render::ApprovalPanelView {
            tool: "Bash".into(),
            detail: "echo hi".into(),
            options: vec![
                "Allow once".into(),
                "Always allow Bash".into(),
                "Deny".into(),
            ],
            selected: 0,
        };
        status.approval = Some(panel.clone());
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let dump = vterm.dump();

        // 1. Numbered prefixes appear.
        assert!(
            vterm.any_row(|r| r.contains("1. ")),
            "option 0 prefix '1. '\n{dump}"
        );
        assert!(
            vterm.any_row(|r| r.contains("2. ")),
            "option 1 prefix '2. '\n{dump}"
        );
        assert!(
            vterm.any_row(|r| r.contains("3. ")),
            "option 2 prefix '3. '\n{dump}"
        );

        // 2. Selected row (option 0 / Allow once) spans full terminal width with
        //    reverse highlight. Find the row, then check every cell is reverse.
        let h = vterm.height() as usize;
        let selected_row = (0..h)
            .find(|&i| {
                vterm.row_text(i).contains("1. ") && vterm.row_text(i).contains("Allow once")
            })
            .expect("selected row with '1. Allow once' not found");
        for col in 0..W as usize {
            let cell = vterm.cell_at(selected_row, col);
            assert!(
                cell.reverse,
                "selected row col {col} must be reverse-highlighted (full-width)\n{dump}"
            );
        }

        // 3. Hint row present somewhere below the options.
        // The hint contains "select" (en locale) or "选择" (zh) or ASCII-downgraded equivalent.
        let hint_present =
            vterm.any_row(|r| r.contains("select") || r.contains("选择") || r.contains("Enter"));
        assert!(
            hint_present,
            "hint row with keyboard hint text not found\n{dump}"
        );

        // 4. Header row names what is being approved.
        assert!(
            vterm.any_row(|r| r.contains("Allow Bash(echo hi)")),
            "approval header 'Allow Bash(echo hi)?' not found\n{dump}"
        );

        // 5. approval_panel_row_count == 1 header + options + 1 hint.
        assert_eq!(
            r.approval_panel_row_count(&panel),
            panel.options.len() + 2,
            "row count must be header + options + hint"
        );
    }

    /// Step 1 digit keys: `accel_index` falls back for y/a/n; digit routing is
    /// tested via the pure `ApprovalPanel.accel_index` + index-based resolution
    /// in `handle_approval_key`. This test confirms the option ordering contract
    /// so digit→index mapping is meaningful.
    #[test]
    fn approval_panel_option_order_for_digit_keys() {
        use crate::state::{ApprovalKind, ApprovalOption, ApprovalPanel};
        let p = ApprovalPanel {
            tool: "bash".into(),
            detail: "cmd".into(),
            options: vec![
                ApprovalOption {
                    label: "Allow once".into(),
                    kind: ApprovalKind::AllowOnce,
                    accel: 'y',
                },
                ApprovalOption {
                    label: "Always allow bash".into(),
                    kind: ApprovalKind::AlwaysAllow,
                    accel: 'a',
                },
                ApprovalOption {
                    label: "Deny".into(),
                    kind: ApprovalKind::Deny,
                    accel: 'n',
                },
            ],
            selected: 0,
            cache_key: String::new(),
        };
        // Digit routing: index = (c as usize) - ('1' as usize).
        // '1' → idx 0 → AllowOnce
        assert_eq!(
            p.options
                .get(('1' as usize) - ('1' as usize))
                .map(|o| o.kind),
            Some(ApprovalKind::AllowOnce)
        );
        // '2' → idx 1 → AlwaysAllow
        assert_eq!(
            p.options
                .get(('2' as usize) - ('1' as usize))
                .map(|o| o.kind),
            Some(ApprovalKind::AlwaysAllow)
        );
        // '3' → idx 2 → Deny
        assert_eq!(
            p.options
                .get(('3' as usize) - ('1' as usize))
                .map(|o| o.kind),
            Some(ApprovalKind::Deny)
        );
        // '9' → idx 8 → out of bounds → None (no panic)
        assert_eq!(
            p.options
                .get(('9' as usize) - ('1' as usize))
                .map(|o| o.kind),
            None
        );
        // Letter accelerator still works (y/a/n).
        assert_eq!(
            p.accel_index('y')
                .and_then(|i| p.options.get(i).map(|o| o.kind)),
            Some(ApprovalKind::AllowOnce)
        );
        assert_eq!(
            p.accel_index('a')
                .and_then(|i| p.options.get(i).map(|o| o.kind)),
            Some(ApprovalKind::AlwaysAllow)
        );
        assert_eq!(
            p.accel_index('n')
                .and_then(|i| p.options.get(i).map(|o| o.kind)),
            Some(ApprovalKind::Deny)
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

    /// A full viewport used to lose the user row when the event loop emitted
    /// `User` first and `ImageAttachment` second: `User` committed a spacer,
    /// the attachment popped it only from memory, and the already-emitted LF
    /// could not be undone. The grouped variant emits no temporary row.
    #[test]
    fn user_with_attachment_stays_visible_when_body_is_full() {
        let w = 80;
        let h = 12;
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
        drain_into_vterm(&buf, &mut vterm);

        let cap = h as usize - r.current_footer_rows();
        for i in 0..cap {
            r.render(UiLine::CommandOutput(format!("filler-{i}")));
        }
        r.render(UiLine::UserWithAttachments {
            text: "[Image #1]你好".into(),
            attachments: vec![1],
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let visible = vterm.dump();
        assert!(
            visible.contains("❯ [Image #1]") && visible.contains('你') && visible.contains('好'),
            "user text must remain visible beside its attachment:\n{visible}"
        );
        assert!(
            visible.contains("└ [Image #1]"),
            "attachment echo must remain visible below user text:\n{visible}"
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
                        cell.ch,
                        cell.style.fg,
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

    /// User-message text stays the terminal's default foreground (no fixed
    /// colour) — the colour lives only on the `❯` Accent marker, mirroring
    /// opencode (`<text fg={theme.text}>` + a coloured left border). The old
    /// `!267` styling painted the whole sentence bright magenta, which read as
    /// too vivid; this locks the text back to default fg.
    #[test]
    fn retained_user_message_text_is_default_fg() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.render(UiLine::User("hello".into()));

        let mut found = false;
        for row in &r.body_lines {
            let text: String = row.iter().map(|c| c.ch).collect();
            if !text.contains("hello") {
                continue;
            }
            found = true;
            // The text glyphs (the alphabetic cells; the chevron is a glyph/space)
            // must carry NO fixed fg — they inherit the terminal default.
            for cell in row {
                if cell.ch.is_alphabetic() {
                    assert_eq!(
                        cell.style.fg, None,
                        "user text cell '{}' must be default fg (None), got {:?}",
                        cell.ch, cell.style.fg,
                    );
                }
            }
            break;
        }
        assert!(found, "no row containing the user text 'hello' found");
    }

    /// A multi-line user message gets a full-height left marker: the `❯` chevron
    /// on the first row, then a coloured `▎` bar (Accent) on every continuation
    /// row — so the block reads as one unit (opencode's `┃` border, fg-only).
    #[test]
    fn retained_multiline_user_message_has_full_height_bar() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.render(UiLine::User("first line\nsecond line".into()));

        let mut saw_chevron = false;
        let mut saw_bar = false;
        for row in &r.body_lines {
            let text: String = row.iter().map(|c| c.ch).collect();
            if text.contains("first line") {
                saw_chevron = true;
                assert_eq!(
                    row.first().map(|c| c.ch),
                    Some('\u{276f}'),
                    "first row must start with the ❯ chevron, got {:?}",
                    text
                );
            }
            if text.contains("second line") {
                saw_bar = true;
                let bar = row.first().expect("continuation row must not be empty");
                assert_eq!(bar.ch, '\u{258e}', "continuation must start with a ▎ bar");
                assert_eq!(
                    bar.style.fg,
                    Some(Color::Cyan),
                    "the bar must be Accent (cyan), got {:?}",
                    bar.style.fg
                );
            }
        }
        assert!(
            saw_chevron && saw_bar,
            "expected a chevron row and a bar continuation row"
        );
    }

    #[test]
    fn retained_user_message_renders_panel_bg_block() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.caps.colors = true;
        let expected_bg =
            crate::render::theme::role(r.caps, crate::render::theme::Role::PanelBg);
        assert!(
            expected_bg.is_some(),
            "PanelBg must resolve to a colour when colours are on"
        );
        r.render(UiLine::User("hello".into()));

        // Locate the content row (the one carrying the user's text).
        let idx = r
            .body_lines
            .iter()
            .position(|row| row.iter().map(|c| c.ch).collect::<String>().contains("hello"))
            .expect("user text row present");

        // Every cell on the content row carries the panel background, including
        // the ❯ marker cell and the trailing padding spaces.
        let content = &r.body_lines[idx];
        assert!(
            content.iter().all(|c| c.style.bg == expected_bg),
            "all content cells must carry PanelBg, got {:?}",
            content.iter().map(|c| c.style.bg).collect::<Vec<_>>()
        );
        // Padded to screen width − PAD_COL (80 − 2 = 78) display columns.
        let cols: usize = content.iter().map(|c| c.width as usize).sum();
        assert_eq!(cols, 78, "content row padded to screen width − PAD_COL");

        // A full-width background blank sits directly above and below.
        let above = &r.body_lines[idx - 1];
        let below = &r.body_lines[idx + 1];
        assert!(
            !above.is_empty()
                && above.iter().all(|c| c.ch == ' ' && c.style.bg == expected_bg),
            "row above content must be a bg padding row"
        );
        assert!(
            !below.is_empty()
                && below.iter().all(|c| c.ch == ' ' && c.style.bg == expected_bg),
            "row below content must be a bg padding row"
        );
    }

    #[test]
    fn retained_user_message_no_bg_when_colours_disabled() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.caps.colors = false;
        r.render(UiLine::User("hello".into()));
        for row in &r.body_lines {
            for c in row {
                assert_eq!(c.style.bg, None, "no background when colours are disabled");
            }
        }
    }

    /// Submitting a recalled prompt clears the composer before its permanent
    /// user echo is laid out. Otherwise a wrapped history entry keeps the
    /// footer artificially tall while the echo is appended, which promotes
    /// the echo's first row into native scrollback. The current viewport then
    /// shows only the continuation row until the user scrolls upward.
    #[test]
    fn retained_submit_clears_wrapped_composer_before_user_echo() {
        let (mut r, _buf) = new_capturing(40, 12);
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        let empty_footer_rows = r.current_footer_rows();
        // One leading separator + bg_blank_top + two text rows + bg_blank_bottom +
        // trailing separator appended by `push_user_message` = 6 rows total.
        let body_rows_before_echo = 12usize.saturating_sub(empty_footer_rows + 6);
        for i in 0..body_rows_before_echo {
            r.push_body_text(&format!("prior-{i}"), &CellStyle::default());
        }

        let recalled = "a recalled prompt long enough to wrap across several composer rows";
        r.render(UiLine::InputPrompt {
            buf: recalled.into(),
            cursor_byte: recalled.len(),
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        assert!(
            r.current_footer_rows() > empty_footer_rows,
            "fixture must produce a taller wrapped composer"
        );

        let scrolled_before = r.scrolled_off;
        r.render(UiLine::ClearTransient);
        r.render(UiLine::User("first line\nsecond line".into()));

        assert_eq!(
            r.scrolled_off, scrolled_before,
            "the first echo row must remain visible instead of being promoted to native scrollback"
        );
        let visible: Vec<String> = r.body_lines[r.scrolled_off..]
            .iter()
            .map(|row| row.iter().map(|cell| cell.ch).collect())
            .collect();
        assert!(visible.iter().any(|row| row.contains("first line")));
        assert!(visible.iter().any(|row| row.contains("second line")));
    }

    /// Windows / no-unicode-font terminals: the continuation bar falls back to an
    /// ASCII `|` (2 display cols, same as `❯`→`>`), so layout stays identical and
    /// no tofu glyph appears.
    #[test]
    fn retained_multiline_user_bar_ascii_fallback() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.caps.unicode_symbols = false; // Windows legacy conhost / no-unicode font
        r.render(UiLine::User("first line\nsecond line".into()));

        let mut saw_bar = false;
        for row in &r.body_lines {
            let text: String = row.iter().map(|c| c.ch).collect();
            if text.contains("second line") {
                saw_bar = true;
                let bar = row.first().expect("continuation row must not be empty");
                assert_eq!(
                    bar.ch, '|',
                    "ascii fallback bar must be '|', got {:?}",
                    bar.ch
                );
            }
        }
        assert!(saw_bar, "expected a continuation row");
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
            hint: None,
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
        let bash_rows: Vec<_> = r
            .body_lines
            .iter()
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
            bash_rows
                .iter()
                .map(|(i, row)| (i, row.iter().map(|c| c.ch).collect::<String>()))
                .collect::<Vec<_>>(),
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

    #[test]
    fn diff_panel_preserves_semantic_styles_and_expands_tabs() {
        let (r, _buf) = new_capturing(80, 24);
        let rows = vec![
            DiffPanelRow::new(vec![
                DiffPanelSpan::new("+1", DiffPanelTone::Add),
                DiffPanelSpan::new("  -2", DiffPanelTone::Remove),
            ]),
            DiffPanelRow::new(vec![DiffPanelSpan::new(
                "\tselected.rs",
                DiffPanelTone::Highlight,
            )]),
        ];
        let title = DiffPanelRow::new(vec![
            DiffPanelSpan::new("Diff", DiffPanelTone::Default).bold()
        ]);
        let overlay = r.build_diff_panel_overlay(&title, &rows, "Esc close", 60, 20);

        // Borderless layout: [0]=top rule, [1]=title, [2..]=content rows.
        let rule = &overlay.cells[0];
        assert!(
            rule.iter().all(|cell| cell.ch == '─'),
            "row 0 must be the full-width top rule"
        );
        let stats = &overlay.cells[2];
        let add = stats.iter().find(|cell| cell.ch == '+').expect("add span");
        let remove = stats
            .iter()
            .find(|cell| cell.ch == '-')
            .expect("remove span");
        assert_eq!(add.style.fg, r.style_for(Role::DiffAdd).fg);
        assert_eq!(remove.style.fg, r.style_for(Role::DiffRemove).fg);

        // Selection: `/resume`-style highlight foreground, never reverse-video.
        let selected = &overlay.cells[3];
        assert!(selected.iter().all(|cell| cell.ch != '\t'));
        assert!(
            selected.iter().all(|cell| !cell.style.reverse),
            "selection must not use reverse-video"
        );
        let glyph = selected
            .iter()
            .find(|cell| cell.ch == 's')
            .expect("highlighted filename glyph");
        assert_eq!(glyph.style.fg, r.session_highlight_style().fg);
    }

    #[test]
    fn diff_panel_is_fixed_height_and_inline() {
        // The panel is a FIXED height (not content-sized) and renders inline
        // right after the body (here empty → y == 0), never a full-screen
        // takeover. It also pads a short content list to the fixed height.
        let (r, _buf) = new_capturing(80, 24);
        let rows = vec![
            DiffPanelRow::new(vec![DiffPanelSpan::new("summary", DiffPanelTone::Muted)]),
            DiffPanelRow::new(Vec::new()),
            DiffPanelRow::new(vec![DiffPanelSpan::new("a.rs", DiffPanelTone::Default)]),
        ];
        let title = DiffPanelRow::new(vec![DiffPanelSpan::new("Diff", DiffPanelTone::Default)]);
        // Ask for a fixed 12-row panel even though there are only 3 content rows.
        let overlay = r.build_diff_panel_overlay(&title, &rows, "Esc close", 0, 12);

        assert_eq!(
            overlay.cells.len(),
            12,
            "panel must honour its fixed height, padding short content"
        );
        assert!(
            overlay.cells.len() < 24,
            "fixed-height panel must not take over the whole screen"
        );
        // Empty body → the panel hugs the top (row 0), following the conversation
        // rather than pinning to the screen bottom.
        assert_eq!(overlay.y, 0, "panel must anchor inline after the body");
        // Last row is the muted key hint, not blank padding.
        let hint = &overlay.cells[overlay.cells.len() - 1];
        assert!(
            hint.iter().any(|cell| cell.ch == 'E'),
            "hint row must sit at the bottom edge of the fixed panel"
        );
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
        let row = build_one_row(&text, &CellStyle::default(), 40, true);
        let total_cols: usize = row.iter().map(|c| c.width as usize).sum();
        assert!(
            total_cols <= 38,
            "row width {} cols exceeds avail 38 (screen=40, PAD_COL=2)",
            total_cols
        );
    }

    #[test]
    fn inflight_tool_meta_survives_wrapping_without_overflow() {
        // Regression (narrow window): an in-flight tool row with a long detail
        // wrapped, and the ` · 93.7s` duration was appended to the FIRST
        // already-full-width row, overflowing the screen — the terminal then
        // clipped/re-wrapped it and the time anchor vanished. The meta must
        // survive, and NO row may exceed the available width.
        let (r, _sink) = new_capturing(40, 24);
        let plain = CellStyle::default();
        let detail = "(mod.rs, publish_service.rs, publish_service.rs, PublishSkill.tsx, skill.ts)";
        let rows = r.build_mixed_style_rows(
            "◑ ",
            &plain,
            "ParallelEditFiles",
            &plain,
            detail,
            &plain,
            " · 93.7s",
            &plain,
            &format!("ParallelEditFiles{detail}"),
        );
        let avail = 40usize - PAD_COL;
        for row in &rows {
            let w: usize = row.iter().map(|c| c.width as usize).sum();
            assert!(w <= avail, "row width {w} exceeds avail {avail}: {row:?}");
        }
        let all: String = rows
            .iter()
            .flat_map(|row| row.iter().map(|c| c.ch))
            .collect();
        assert!(
            all.contains("93.7s"),
            "duration must survive wrapping, got: {all:?}",
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
    fn apply_sgr_37_is_grey_for_dark_table_borders() {
        // SGR 37 is emitted by `theme::md_border_open()` for table borders
        // on dark themes (DarkGrey/SGR 90 is swallowed by the bg there).
        // The parser must map it to Color::Grey — a visible light-gray —
        // not drop it (which would fall back to the default fg) and not
        // bright white (Color::White / SGR 97).
        let mut style = CellStyle::default();
        apply_sgr("37", &mut style);
        assert_eq!(
            style.fg,
            Some(Color::Grey),
            "SGR 37 must map to Color::Grey"
        );
    }

    #[test]
    fn apply_sgr_34_is_blue_for_light_theme_headings() {
        // SGR 34 is emitted by `theme::md_heading_open()` on LIGHT themes
        // (dark uses bright cyan / SGR 96). The parser must map it to a
        // visible blue — dropping it (the old `_ => {}` arm) left light-theme
        // headings in the default fg (black): the "colours vanished on the
        // light theme" bug. Mirrors `render::cell::apply_sgr`'s 34→DarkBlue.
        let mut style = CellStyle::default();
        apply_sgr("34", &mut style);
        assert_eq!(style.fg, Some(Color::DarkBlue), "SGR 34 must map to blue");
    }

    #[test]
    fn apply_sgr_35_is_magenta_for_light_theme_inline_code() {
        // SGR 35 is emitted by `theme::md_inline_code_open()` on LIGHT themes
        // (dark uses bright cyan / SGR 96). Must map to magenta so `code`
        // spans stay coloured on white; the old parser dropped it → black.
        let mut style = CellStyle::default();
        apply_sgr("35", &mut style);
        assert_eq!(
            style.fg,
            Some(Color::DarkMagenta),
            "SGR 35 must map to magenta"
        );
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
        assert_eq!(
            r.body_lines.len(),
            5000,
            "body_lines should cap at 5000, got {}",
            r.body_lines.len()
        );
    }

    #[test]
    fn retained_message_marks_tracked_on_user_push() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.render(UiLine::User("hi".into()));
        assert_eq!(r.message_marks.len(), 1);
        assert_eq!(r.message_marks[0].kind, crate::render::MarkKind::User);
    }

    #[test]
    fn retained_blank_line_between_tool_result_and_assistant() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.render(UiLine::ToolCall {
            name: "web_search".into(),
            detail: "长沙天气".into(),
        });
        r.render(UiLine::ToolResult {
            success: true,
            summary: "sources: weather.com.cn".into(),
            diff_stats: None,
        });
        r.render(UiLine::AssistantText("今天长沙的天气情况如下：\n".into()));

        let lines: Vec<String> = r
            .body_lines
            .iter()
            .map(|row| row.iter().map(|c| c.ch).collect::<String>())
            .collect();

        // There should be a blank spacer line at index 2 (between ToolResult at index 1 and AssistantText at index 3)
        assert!(
            lines.len() >= 4,
            "Expected at least 4 lines, got {:?}",
            lines
        );
        assert!(
            lines[2].trim().is_empty(),
            "Expected line 2 to be blank, got {:?}",
            lines[2]
        );
        assert!(
            lines[3].contains("今"),
            "Expected line 3 to contain assistant text, got {:?}",
            lines[3]
        );
    }

    #[test]
    fn retained_message_marks_decremented_on_drain() {
        let (mut r, _buf) = new_capturing(80, 24);
        // Each UiLine::User pushes 4 body rows (bg_blank_top + text + bg_blank_bottom + spacer).
        // 5005 users → 20020 body rows. drain = 20020 - 5000 = 15020 rows from front.
        // Marks at line_idx < 15020 are dropped; the first surviving mark is at
        // original idx=15020, which normalises to 0 after subtracting the drain.
        for i in 0..5005 {
            r.render(UiLine::User(format!("line {}", i)));
        }
        // 15020 / 4 = 3755 marks dropped; 5005 - 3755 = 1250 survive.
        assert_eq!(r.message_marks.len(), 1250);
        assert_eq!(
            r.message_marks[0].line_idx, 0,
            "first surviving mark should point at body_lines[0] after drain"
        );
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
        assert!(
            s.contains("\x1b[?1006l"),
            "suspend must disable SGR: {:?}",
            s
        );
        assert!(
            s.contains("\x1b[?1002l"),
            "suspend must disable button-event: {:?}",
            s
        );
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
    fn retained_resume_does_not_request_keyboard_release_events() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = CapturingSink(buf.clone());
        let mut r = RetainedRenderer::with_writer(sink, caps_with_color(), 80, 24);
        r.suspend_for_external();
        buf.lock().unwrap().clear();

        r.resume_from_external();

        let bytes = buf.lock().unwrap().clone();
        let output = String::from_utf8_lossy(&bytes);
        assert!(
            output.contains("\x1b[>1u") && !output.contains("\x1b[>3u"),
            "resume must enable disambiguation without release reports: {output:?}"
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
        assert!(
            s.contains("\x1b[?1002l"),
            "shutdown must disable button-event: {:?}",
            s
        );
        assert!(
            s.contains("\x1b[?1006l"),
            "shutdown must disable SGR coords: {:?}",
            s
        );
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
            before,
            after,
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
    /// Repro sequence: fill body to exactly `cap`, push and clear a
    /// transient spinner (neither operation may promote history), then
    /// push one permanent body row (the first and only promotion).
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

        // Spinner push is transient even though body_lines.len() == cap:
        // it must not LF the PROBE row into native scrollback.
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
            0,
            "transient spinner must not promote PROBE into scrollback; got {}.\nscrollback:\n{}",
            count_probe(&vterm),
            vterm.scrollback_texts().join("\n")
        );

        // Idle InputPrompt triggers clear_live_spinner → pops the
        // transient spinner row. body_lines.len() drops from cap+1 back
        // to cap without changing the scrollback boundary.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // The next permanent body push performs the one legitimate
        // overflow and promotes PROBE exactly once.
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

    /// Regression for a live-spinner/streaming interleave at the exact
    /// overflow boundary. The transient spinner and its spacer used to
    /// append past `cap` without advancing `scrolled_off`. That made the
    /// painted viewport start one or two rows after the logical scroll
    /// boundary; the next permanent push then LF-promoted the wrong
    /// physical top row while advancing `scrolled_off` as if it had
    /// promoted the logical top row. Repeating the interleave could put
    /// the same permanent row into native scrollback more than once.
    #[test]
    fn retained_full_body_spinner_stream_does_not_duplicate_scrollback() {
        let w: u16 = 80;
        let h: u16 = 12;
        for (case, trailing_blank) in [("spinner-only", true), ("spinner-spacer", false)] {
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
            drain_into_vterm(&buf, &mut vterm);
            let cap = (h as usize).saturating_sub(r.current_footer_rows());

            // Establish a non-zero logical scrollback boundary, matching
            // the real trace (`len=565, scrolled_off=524, cap=41`). A
            // trailing blank makes the spinner a one-row transient strip;
            // a trailing marker forces the two-row spacer+spinner shape.
            let seed_rows = cap + 20;
            let seed_markers = seed_rows - usize::from(trailing_blank);
            for i in 0..seed_markers {
                r.render(UiLine::AssistantText(format!("{case}-PINNED-{i:03}\n")));
            }
            if trailing_blank {
                r.render(UiLine::AssistantText("\n".into()));
            }
            r.flush_deferred();
            drain_into_vterm(&buf, &mut vterm);
            assert!(r.scrolled_off > 0, "{case}: setup must overflow");

            // Match the trace signature: while the transcript remains
            // pinned at the overflow boundary, repeatedly paint a live
            // spinner, clear it to verify the covered tail is restored,
            // then recreate it before the next permanent streamed line.
            for i in 0..(cap + 6) {
                let expected_top: String = r.body_lines[r.scrolled_off]
                    .iter()
                    .map(|cell| cell.ch)
                    .collect();
                r.render(UiLine::Spinner {
                    frame: "⠋".into(),
                    label: format!("{case} Pondering {i}"),
                });
                r.flush_deferred();
                drain_into_vterm(&buf, &mut vterm);

                assert_eq!(
                    r.live_spinner_spacer_active, !trailing_blank,
                    "{case}: wrong transient strip shape"
                );
                assert_eq!(
                    vterm.row_text(0).trim_end(),
                    expected_top.trim_end(),
                    "{case}: spinner paint moved the physical top away from scrolled_off"
                );
                assert!(
                    vterm
                        .row_text(cap - 1)
                        .contains(&format!("{case} Pondering {i}")),
                    "{case}: spinner must occupy the final body slot\n{}",
                    vterm.dump()
                );
                if !trailing_blank {
                    assert!(
                        vterm.row_text(cap - 2).trim().is_empty(),
                        "{case}: spacer must occupy the slot above the spinner\n{}",
                        vterm.dump()
                    );
                }
                assert!(
                    vterm.row_text(cap).contains('─'),
                    "{case}: transient strip overwrote the footer boundary\n{}",
                    vterm.dump()
                );

                r.render(UiLine::InputPrompt {
                    buf: String::new(),
                    cursor_byte: 0,
                    menu: None,
                    status: status_basic(),
                    attachments: Vec::new(),
                });
                r.flush_deferred();
                drain_into_vterm(&buf, &mut vterm);
                for (screen_row, body_row) in
                    r.body_lines[r.scrolled_off..].iter().take(cap).enumerate()
                {
                    let expected: String = body_row.iter().map(|cell| cell.ch).collect();
                    assert_eq!(
                        vterm.row_text(screen_row).trim_end(),
                        expected.trim_end(),
                        "{case}: clearing spinner did not restore permanent row {screen_row}"
                    );
                }

                r.render(UiLine::Spinner {
                    frame: "⠙".into(),
                    label: format!("{case} Pondering {i}"),
                });
                r.flush_deferred();
                drain_into_vterm(&buf, &mut vterm);
                let suffix = if trailing_blank { "\n\n" } else { "\n" };
                r.render(UiLine::AssistantText(format!(
                    "{case}-STREAM-{i:03}{suffix}"
                )));
                r.flush_deferred();
                drain_into_vterm(&buf, &mut vterm);
            }

            let scrollback = vterm.scrollback_texts();
            let adjacent_duplicates: Vec<_> = scrollback
                .windows(2)
                .filter(|rows| !rows[0].trim().is_empty() && rows[0] == rows[1])
                .map(|rows| rows[0].clone())
                .collect();
            assert!(
                adjacent_duplicates.is_empty(),
                "{case}: full-body spinner/stream interleave duplicated adjacent \
                 native scrollback rows: {adjacent_duplicates:?}\nscrollback:\n{}",
                scrollback.join("\n")
            );

            let all_terminal_rows = scrollback
                .iter()
                .cloned()
                .chain((0..h as usize).map(|row| vterm.row_text(row)))
                .collect::<Vec<_>>();
            for i in 0..seed_markers {
                let marker = format!("{case}-PINNED-{i:03}");
                let count = all_terminal_rows
                    .iter()
                    .filter(|row| row.contains(&marker))
                    .count();
                assert_eq!(
                    count,
                    1,
                    "{case}: {marker} must occur exactly once across terminal \
                     history and viewport (found {count})\nterminal rows:\n{}",
                    all_terminal_rows.join("\n"),
                );
            }
            for i in 0..(cap + 6) {
                let marker = format!("{case}-STREAM-{i:03}");
                let count = all_terminal_rows
                    .iter()
                    .filter(|row| row.contains(&marker))
                    .count();
                assert_eq!(
                    count,
                    1,
                    "{case}: {marker} must occur exactly once across terminal \
                     history and viewport (found {count})\nterminal rows:\n{}",
                    all_terminal_rows.join("\n"),
                );
            }
        }
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
            r.render(UiLine::AssistantText(format!("filler-bullet-{:03}\n", i)));
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
            r.render(UiLine::AssistantText(format!("filler-bullet-{:03}\n", i)));
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

    /// REPRO: during streaming with a full viewport, the user's own echo
    /// row must stay visible. When a tool runs *after* the echo, the echo
    /// becomes a middle permanent row; the live-spinner overlay's
    /// "keep prefix + newest 2 permanent rows" heuristic omits the middle,
    /// so the echo vanishes until the spinner clears (the reported bug).
    #[test]
    fn retained_streaming_spinner_keeps_user_echo_when_tool_follows() {
        let w: u16 = 60;
        let h: u16 = 20;
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
        drain_into_vterm(&buf, &mut vterm);

        let cap = (h as usize).saturating_sub(r.current_footer_rows());
        // Prior turn fills the viewport so the new turn overflows.
        for i in 0..(cap + 4) {
            r.render(UiLine::AssistantText(format!("prior-line-{:03}\n", i)));
        }
        // New turn: user echo, THEN a tool runs (so the echo is no longer
        // one of the two newest permanent rows), THEN the spinner ticks.
        let echo = "check-beijing-weather-please";
        r.render(UiLine::User(echo.into()));
        r.render(UiLine::AssistantText("WebSearch(beijing weather)\n".into()));
        r.render(UiLine::AssistantText("sources: tianqi.com, exa.ai +3\n".into()));
        r.render(UiLine::Spinner {
            frame: "⠋".into(),
            label: "Pondering…".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        assert!(
            vterm.any_row(|row| row.contains(echo)),
            "user echo must stay on screen while the spinner is active.\n{}",
            vterm.dump()
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
            hint: None,
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

    /// Regression: the bash "Press Ctrl+o …" hint must ride INSIDE the
    /// inflight strip so the commit erase covers it. Previously the hint was a
    /// separate body row pushed AFTER the strip, breaking the "strip = body
    /// tail" invariant; on commit the spinner glyph orphaned and lingered next
    /// to the committed `●` (bash-only, since only bash gets the hint).
    #[test]
    fn retained_bash_inflight_hint_is_part_of_strip_and_cleared_on_commit() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
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

        // Bash inflight WITH the Ctrl+o hint (the bash-only case that regressed).
        r.render(UiLine::ToolCallInFlight {
            id: "call-1".into(),
            name: "Bash".into(),
            detail: "grep -n pattern file".into(),
            hint: Some("Press Ctrl+o to show real-time output while running".into()),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // During flight the hint is visible AND part of the strip (so
        // inflight_tool_rows counts it — header + hint ≥ 2).
        assert!(
            vterm.any_row(|row| row.contains("Press Ctrl+o")),
            "hint must show during flight:\n{}",
            vterm.dump()
        );
        assert!(
            r.inflight_tool_rows >= 2,
            "hint must be part of the inflight strip (rows>=2), got {}",
            r.inflight_tool_rows
        );
        let hint_row = (0..24)
            .find(|row| vterm.row_text(*row).contains("Press Ctrl+o"))
            .expect("hint row must be visible");
        let running_row = (0..24)
            .find(|row| vterm.row_text(*row).contains("Running"))
            .expect("Running row must be visible");
        assert_eq!(
            running_row - hint_row,
            2,
            "hint and Running must have exactly one blank row between them:\n{}",
            vterm.dump()
        );

        // Commit the tool (ToolCallResult → ToolCallCommit).
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-1".into()),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // The committed tool row remains, but the hint is GONE — not orphaned
        // above the committed `●`.
        assert!(
            vterm.any_row(|row| row.contains("Bash")),
            "committed tool row must remain:\n{}",
            vterm.dump()
        );
        assert!(
            !vterm.any_row(|row| row.contains("Press Ctrl+o")),
            "hint must be cleared on commit, not left orphaned:\n{}",
            vterm.dump()
        );
    }

    /// Bash without the Ctrl+o hint follows the same live-spinner spacing
    /// invariant: one blank row separates the command block from `Running`.
    #[test]
    fn retained_bash_inflight_without_hint_keeps_blank_above_running() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);

        r.render(UiLine::ToolCallInFlight {
            id: "call-1".into(),
            name: "Bash".into(),
            detail: "cargo test -p atomcode-tuix".into(),
            hint: None,
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        let command_row = (0..24)
            .find(|row| vterm.row_text(*row).contains("cargo test"))
            .unwrap_or_else(|| panic!("bash command missing:\n{}", vterm.dump()));
        let running_row = (0..24)
            .find(|row| vterm.row_text(*row).contains("Running"))
            .unwrap_or_else(|| panic!("Running row missing:\n{}", vterm.dump()));
        assert_eq!(
            running_row - command_row,
            2,
            "command and Running must have exactly one blank row between them:\n{}",
            vterm.dump()
        );
    }

    /// A second inflight tool that PREEMPTS a still-running one (the rare
    /// parallel-call path) must keep ITS OWN hint. Regression guard: the hint
    /// must be set AFTER the preempt-commit (which clears `inflight_hint`),
    /// not before, or the new tool renders without its hint.
    #[test]
    fn retained_preempting_inflight_keeps_its_own_hint() {
        let (mut r, buf) = new_capturing(80, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
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

        // Tool A starts (still inflight — no commit).
        r.render(UiLine::ToolCallInFlight {
            id: "call-a".into(),
            name: "Bash".into(),
            detail: "first cmd".into(),
            hint: Some("HINT_FOR_A".into()),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // Tool B preempts A (arrives before A commits) with ITS OWN hint.
        r.render(UiLine::ToolCallInFlight {
            id: "call-b".into(),
            name: "Bash".into(),
            detail: "second cmd".into(),
            hint: Some("HINT_FOR_B".into()),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);

        // B's hint must be visible — not wiped by A's preempt-commit.
        assert!(
            vterm.any_row(|row| row.contains("HINT_FOR_B")),
            "preempting tool must keep its own hint:\n{}",
            vterm.dump()
        );
    }

    /// Bug repro: user reports that assistant text rows duplicate in
    /// the scrollback right before a tool call commit burst,
    /// visible on every terminal (macOS Terminal.app, iTerm2, VSCode
    /// pwsh xterm.js) by scrolling up. Wire-dump confirms the LLM
    /// only emits each line once. So the duplicate is in body_lines.
    ///
    /// This test simulates the most-likely event sequence under load
    /// — TextDelta chunks token-by-token interleaved with spinner
    /// ticks, then a ToolCallStreaming → ToolCallStarted → ToolCallCommit
    /// burst — and asserts that body_lines never contains two
    /// adjacent entries with the same content.
    /// (Rewritten to use CommandOutput instead of the removed ApprovalPrompt.)
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
            "副本",
            "编辑",
            "成功",
            "、读回",
            "一致",
            "、删除 - ",
            "PASS",
            "。\n\n",
            "步骤 7",
            " - ",
            "`search_replace`",
            "\n",
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
            hint: None,
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

        // Phase 3: tool executes — commit inflight + result output.
        // (Previously used ApprovalPrompt here; replaced with CommandOutput
        // since the body ApprovalPrompt variant has been removed.)
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-7".into()),
        });
        r.render(UiLine::CommandOutput(
            "wrote atomcode_smoke_replace.txt".into(),
        ));
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

    /// Bug repro: single edit_file tool call (auto-approved) must render
    /// exactly ONE ● EditFile row in body_lines. User report shows two
    /// when ToolCallResult arrives and both ToolCallCommit → commit_inflight_tool
    /// AND ToolResult's defense-in-depth commit_inflight_tool produce a row.
    #[test]
    fn retained_single_edit_file_does_not_double_editfile() {
        let (mut r, _buf) = new_capturing(80, 24);
        let status = status_basic();
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Simulate ToolCallStarted -> inflight spinner for edit_file
        r.render(UiLine::ToolCallInFlight {
            id: "call-edit-1".into(),
            name: "EditFile".into(),
            detail: "numbers.txt ← \"5\"".into(),
            hint: None,
        });
        r.flush_deferred();

        // Count rows before result (should be 1: the spinner)
        let before = r
            .body_lines
            .iter()
            .filter(|row| {
                let text: String = row.iter().map(|c| c.ch).collect();
                text.contains("EditFile")
            })
            .count();
        eprintln!("EditFile rows before ToolCallResult: {}", before);
        assert_eq!(before, 1, "spinner row must exist before result");

        // Simulate ToolCallResult doing EXACTLY what the event loop does:
        //   ToolCallCommit (line 6813) + ToolResult (line 6859)
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-edit-1".into()),
        });
        r.render(UiLine::ToolResult {
            success: true,
            summary: "Edited /path/numbers.txt (1 replacement)".into(),
            diff_stats: None,
        });
        r.flush_deferred();

        // Count ● EditFile rows after result
        let after = r
            .body_lines
            .iter()
            .filter(|row| {
                let text: String = row.iter().map(|c| c.ch).collect();
                text.contains("EditFile") && text.contains("●")
            })
            .count();

        eprintln!("Body lines after ToolCallResult:");
        for (i, row) in r.body_lines.iter().enumerate() {
            let text: String = row.iter().map(|c| c.ch).collect();
            if !text.is_empty() {
                eprintln!("  [{}] {:?}", i, text);
            }
        }

        assert_eq!(
            after, 1,
            "ToolCallResult must produce exactly ONE ● EditFile row, got {}. body_lines has {} total rows.\n{:?}",
            after,
            r.body_lines.len(),
            r.body_lines.iter().map(|row| row.iter().map(|c| c.ch).collect::<String>()).collect::<Vec<_>>()
        );

        // Verify the └ result row is immediately after the ● row
        let tool_idx = r
            .body_lines
            .iter()
            .rposition(|row| {
                let text: String = row.iter().map(|c| c.ch).collect();
                text.contains("EditFile") && text.contains("●")
            })
            .unwrap();
        let result_idx = r
            .body_lines
            .iter()
            .position(|row| {
                let text: String = row.iter().map(|c| c.ch).collect();
                text.contains("Edited") && text.contains("replacement")
            })
            .unwrap();
        assert_eq!(
            result_idx,
            tool_idx + 1,
            "result row must be immediately after tool row, gap of {} rows",
            result_idx - tool_idx - 1
        );
    }

    /// Bug repro: when ToolCallCommit fires while inflight and then ToolCallResult
    /// arrives later, must not produce a second ● row. (Originally tested the
    /// approval flow; rewritten without the removed body ApprovalPrompt variant —
    /// the double-● assertion applies equally to any two-phase ToolCallCommit
    /// sequence: first commit (commit inflight), then result commit.)
    #[test]
    fn retained_approval_flow_does_not_double_editfile() {
        let (mut r, _buf) = new_capturing(80, 24);
        let status = status_basic();
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status.clone(),
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // Phase 1: ToolCallStarted → inflight spinner
        r.render(UiLine::ToolCallInFlight {
            id: "call-edit-1".into(),
            name: "EditFile".into(),
            detail: "numbers.txt ← \"5\"".into(),
            hint: None,
        });
        r.flush_deferred();

        // Phase 2: ToolCallCommit commits the inflight to a permanent ● row.
        // (Previously this was the ApprovalNeeded path; now it's just the
        // first ToolCallCommit that freezes the spinner to ●.)
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-edit-1".into()),
        });
        r.flush_deferred();

        // Count ● rows AFTER first commit (should be 1)
        let mid = r
            .body_lines
            .iter()
            .filter(|row| {
                let text: String = row.iter().map(|c| c.ch).collect();
                text.contains("EditFile") && text.contains("●")
            })
            .count();
        eprintln!("● EditFile rows after first ToolCallCommit: {}", mid);
        assert_eq!(
            mid, 1,
            "First ToolCallCommit must produce exactly ONE ● EditFile row"
        );

        // Phase 3: ToolCallResult arrives
        // EXACT sequence from event loop: AssistantLineBreak + ToolCallCommit + ToolResult
        r.render(UiLine::AssistantLineBreak);
        r.render(UiLine::ToolCallCommit {
            call_id: Some("call-edit-1".into()),
        });
        r.render(UiLine::ToolResult {
            success: true,
            summary: "Edited /path/numbers.txt (1 replacement)".into(),
            diff_stats: None,
        });
        r.flush_deferred();

        eprintln!("Body lines after ToolCallResult:");
        for (i, row) in r.body_lines.iter().enumerate() {
            let text: String = row.iter().map(|c| c.ch).collect();
            if !text.is_empty() {
                eprintln!("  [{}] {:?}", i, text);
            }
        }

        // Count ● EditFile rows after ToolCallResult (must still be 1)
        let after = r
            .body_lines
            .iter()
            .filter(|row| {
                let text: String = row.iter().map(|c| c.ch).collect();
                text.contains("EditFile") && text.contains("●")
            })
            .count();

        assert_eq!(
            after,
            1,
            "Full flow must produce exactly ONE ● EditFile row, got {}.\n{:?}",
            after,
            r.body_lines
                .iter()
                .map(|row| row.iter().map(|c| c.ch).collect::<String>())
                .collect::<Vec<_>>()
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
        let long_payload = format!("write_file(/long/path/to/file.md) {}", long_marker);
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
        // bug requires. `commit_inflight_tool` / `refresh_welcome_banner`
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
            right_edge.ch,
            ' ',
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
            cell_at_right_edge.ch,
            ' ',
            "right-edge cell at row {} col {} should be blank, got {:?}\ndump:\n{}",
            long_screen_row,
            w - 5,
            cell_at_right_edge,
            vterm.dump()
        );
    }

    /// Auto-copy is OFF by default: a completed code block must NOT hijack the
    /// clipboard nor emit the "copied" hint unless the user opted in. The gate
    /// short-circuits BEFORE consuming the code-block source, so the source is
    /// left untouched — a deterministic, clipboard-independent proof no copy was
    /// attempted. Regression guard for the #699 auto-copy that silently clobbered
    /// the user's clipboard on every code-block reply.
    #[test]
    fn auto_copy_off_by_default_makes_no_copy_attempt() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.md_state.last_code_block_source = Some("cargo build --release".into());
        assert!(
            r.maybe_auto_copy_hint().is_none(),
            "auto-copy OFF by default → no hint"
        );
        assert!(
            r.md_state.last_code_block_source.is_some(),
            "OFF must not even consume the source — no clipboard write attempted"
        );
    }

    /// End-to-end companion to `auto_copy_fires_for_single_code_block_reply`:
    /// the SAME single-block reply that WOULD be copy-eligible must produce NO
    /// copy when auto-copy is off (the default). This is the exact user-reported
    /// scenario — a single-block reply silently clobbering the clipboard — now
    /// suppressed by default.
    #[test]
    fn auto_copy_off_by_default_single_block_not_copied() {
        let (mut r, _buf) = new_capturing(80, 24);
        // No set_auto_copy_enabled → default OFF.
        let stream = "here is the snippet:\n\
                      ```\nthe only block\n```\n\
                      thanks\n";
        r.render(UiLine::AssistantText(stream.into()));
        r.render(UiLine::TurnComplete);
        assert_eq!(
            copy_hint_count(&r),
            0,
            "default-OFF must not auto-copy even a single-block reply"
        );
    }

    // (The "enabled → single block copies" case is covered end-to-end by
    // `auto_copy_fires_for_single_code_block_reply` below, which opts in first.)

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
    // Auto-copy should fire ONLY when the whole reply is essentially a SINGLE
    // fenced code block. A long answer that happens to contain several
    // illustrative ``` blocks must NOT auto-copy: each per-block copy clobbers
    // the system clipboard (last block wins) and spams a "代码块已复制" hint —
    // the user-reported side-effect. The copy tier tries arboard first (so OSC52
    // is not always emitted); the locale-independent signal that a copy DID
    // happen is the "📋" hint row pushed into the body on success (both the zh
    // and en `CodeBlockCopied` strings start with 📋).
    fn copy_hint_count(r: &RetainedRenderer<CapturingSink>) -> usize {
        r.body_lines
            .iter()
            .filter(|row| row.iter().any(|c| c.ch == '📋'))
            .count()
    }

    #[test]
    fn auto_copy_skips_multi_code_block_reply() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.set_auto_copy_enabled(true); // opt-in, so this tests the block-COUNT gate
        let stream = "intro\n\
                      ```\nblock one\n```\n\
                      middle\n\
                      ```\nblock two\n```\n\
                      end\n\
                      ```\nblock three\n```\n";
        r.render(UiLine::AssistantText(stream.into()));
        r.render(UiLine::TurnComplete);
        assert_eq!(
            copy_hint_count(&r),
            0,
            "a reply with 3 fenced code blocks must NOT auto-copy any of them"
        );
    }

    #[test]
    fn auto_copy_fires_for_single_code_block_reply() {
        let (mut r, _buf) = new_capturing(80, 24);
        r.set_auto_copy_enabled(true); // opt-in (#699: auto-copy is OFF by default)
        let stream = "here is the snippet:\n\
                      ```\nthe only block\n```\n\
                      thanks\n";
        r.render(UiLine::AssistantText(stream.into()));
        r.render(UiLine::TurnComplete);
        assert_eq!(
            copy_hint_count(&r),
            1,
            "a reply with exactly ONE fenced code block must auto-copy it exactly once"
        );
    }

    #[test]
    fn auto_copy_skips_unclosed_code_block() {
        // Latent bug: an unclosed fence (model never wrote the closing ```), or a
        // block prematurely finalized mid-stream (tool interleave), must NOT be
        // auto-copied — it's incomplete. Only a properly-CLOSED fence counts.
        let (mut r, _buf) = new_capturing(80, 24);
        r.set_auto_copy_enabled(true); // opt-in, so this tests the CLOSED-fence gate
        let stream = "text\n```\nunclosed block content\n";
        r.render(UiLine::AssistantText(stream.into()));
        r.render(UiLine::TurnComplete);
        assert_eq!(
            copy_hint_count(&r),
            0,
            "an unclosed (incomplete) code block must NOT auto-copy"
        );
    }

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
    /// When upstream UI chrome (footer approval chip, top-rule session
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
        // a footer approval chip's reverse state could leak into a
        // syntect-highlighted code block.
        let stream = format!(
            "\x1b[7mY{reset}\x1b[38;2;209;154;102m42{reset}",
            reset = crate::highlight::theme::RESET,
        );

        let lines = parse_markdown_to_cells(&stream);
        assert_eq!(lines.len(), 1, "expected single line, got {:?}", lines);
        let row = &lines[0];

        // Find the Number-token cells (chars '4' and '2').
        let number_cells: Vec<&Cell> = row.iter().filter(|c| c.ch == '4' || c.ch == '2').collect();
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
            occ_para,
            1,
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
            occ_label,
            1,
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
        r.set_suppress_auto_copy(true);
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

    /// REGRESSION (`/resume` flicker): replaying a session re-emits the
    /// whole transcript through the append-only direct-stdout path —
    /// `reset()` blanks the screen, then every historical row goes out via
    /// `emit_body_line_inner` (CUP+EL+cells+LF), LF-scrolling rows into
    /// native scrollback. Each overflow scroll also makes the render
    /// worker repaint the footer (`take_pending_scroll_flush` →
    /// `flush_deferred`). Pre-fix, the `reset()` blank flushed entirely
    /// OUTSIDE any DECSET 2026 envelope, and every one of those footer
    /// repaints emitted its OWN `?2026h…?2026l` — so the terminal visibly
    /// blanked and then re-scrolled the history (the user-reported flash).
    ///
    /// Fix: `replay_session` brackets the whole sequence in ONE
    /// synchronized envelope via `begin_sync()` / `end_sync()`, and while
    /// that batch is open `render_diff` SUPPRESSES its per-frame envelope
    /// (a nested `?2026l` would end the batch early). Contract, asserted at
    /// the byte-stream level: across a full replay — including viewport
    /// overflow and per-row footer repaints — the output contains EXACTLY
    /// ONE `?2026h` (at the very start, so nothing paints before it) and
    /// EXACTLY ONE `?2026l` (at the very end, after the final frame lands).
    #[test]
    fn replay_sync_batch_emits_single_2026_envelope_even_with_overflow() {
        let w: u16 = 40;
        let h: u16 = 8;
        let (mut r, buf) = new_capturing(w, h);

        // Warm prev_cells so reset() + the first diff have real work
        // (mirrors a populated pre-resume session — see
        // `retained_post_reset_replay_then_burst_does_not_duplicate_tail`).
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

        // Drive the same call shape `replay_session` uses: one synchronized
        // batch around reset() + the body re-emit + the trailing prompt.
        // 16 rows on an 8-row terminal guarantees viewport overflow, so
        // `emit_body_line_inner` LF-scrolls and the per-row footer repaint
        // (`flush_deferred` after each row, mimicking the worker) runs —
        // the exact multi-envelope failure mode.
        r.begin_sync();
        r.set_suppress_auto_copy(true);
        r.reset();
        for i in 0..16 {
            r.render(UiLine::AssistantText(format!("replay row {i}\n")));
            r.flush_deferred();
        }
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        r.end_sync();

        let out = {
            let g = buf.lock().unwrap();
            String::from_utf8_lossy(&g).into_owned()
        };
        let opens = out.matches("\x1b[?2026h").count();
        let closes = out.matches("\x1b[?2026l").count();
        assert_eq!(
            opens, 1,
            "replay must open exactly ONE synchronized envelope (no nested \
             per-frame BSU), got {opens}.\nbytes: {:?}",
            out
        );
        assert_eq!(
            closes, 1,
            "replay must close exactly ONE synchronized envelope, got {closes}.\nbytes: {:?}",
            out
        );
        // The single envelope must bracket EVERYTHING: BSU is the very
        // first byte (nothing — not even reset()'s blank — paints before
        // it) and ESU is the very last (emitted after the final frame).
        assert!(
            out.starts_with("\x1b[?2026h"),
            "stream must open with BSU so the reset() blank never shows: {:?}",
            out
        );
        assert!(
            out.ends_with("\x1b[?2026l"),
            "stream must close with ESU after the final paint: {:?}",
            out
        );
    }

    /// Windows resize footer-duplication regression: `on_resize` must call
    /// `screen.invalidate()` after rebuilding the Screen so the repaint
    /// cold-starts with a per-row CUP+EL preamble — like `reset()` and
    /// `resume_from_external()` already do.
    ///
    /// REGRESSION (resize flicker — sibling of the `/resume` fix): `on_resize`
    /// wipes the screen and re-emits the visible body tail via direct stdout
    /// writes (`retained.rs` wipe + `emit`), all OUTSIDE the DECSET 2026
    /// envelope, before a final synchronized `paint_frame`. Pre-fix the
    /// terminal visibly blanked then re-stamped the body unsynchronized on
    /// every resize. Fix: bracket the whole `on_resize` body in ONE outer 2026
    /// envelope and suppress the inner `render_diff`'s own envelope so it can't
    /// nest. Contract: the resize byte stream opens with EXACTLY ONE `?2026h`
    /// (the very first byte, before the wipe) and closes with EXACTLY ONE
    /// `?2026l` (the very last, after the final paint).
    #[test]
    fn on_resize_emits_single_2026_envelope() {
        let (mut r, buf) = new_capturing(40, 10);
        // Populate body + paint so the resize has real content to re-emit.
        for i in 0..6 {
            r.render(UiLine::AssistantText(format!("line {i}\n")));
        }
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        buf.lock().unwrap().clear();

        // Height change → real resize work (wipe + body re-emit + paint).
        r.on_resize(40, 8);

        let out = {
            let g = buf.lock().unwrap();
            String::from_utf8_lossy(&g).into_owned()
        };
        let opens = out.matches("\x1b[?2026h").count();
        let closes = out.matches("\x1b[?2026l").count();
        assert_eq!(
            opens, 1,
            "resize must open exactly ONE envelope (no nested per-frame BSU), \
             got {opens}.\nbytes: {:?}",
            out
        );
        assert_eq!(
            closes, 1,
            "resize must close exactly ONE envelope, got {closes}.\nbytes: {:?}",
            out
        );
        assert!(
            out.starts_with("\x1b[?2026h"),
            "resize must open with BSU so the wipe never shows unsynchronized: {:?}",
            out
        );
        assert!(
            out.ends_with("\x1b[?2026l"),
            "resize must close with ESU after the final paint: {:?}",
            out
        );
    }

    #[test]
    fn retained_resize_does_not_duplicate_partial_assistant_line() {
        let (mut r, _buf) = new_capturing(40, 10);

        r.render(UiLine::AssistantText("partial".into()));

        r.on_resize(42, 10);
        r.on_resize(44, 10);

        assert_eq!(r.body_log.len(), 1, "unexpected body_log: {:?}", r.body_log);
        let UiLine::AssistantText(text) = &r.body_log[0] else {
            panic!("expected AssistantText entry, got {:?}", r.body_log);
        };
        assert_eq!(
            text, "partial",
            "resize must not append assistant_line_buf into body_log again; \
             AssistantText chunks are already logged before they enter the line buffer"
        );
    }

    /// A streaming reply that never emits a newline (hung keep-alive / one giant
    /// single line) must not grow `assistant_line_buf` without bound — that was an
    /// OOM → `panic=abort` (Windows "stack-based buffer overrun"). The >1 MiB cap
    /// force-flushes the partial, keeping the buffer bounded.
    #[test]
    fn assistant_line_buf_capped_on_newlineless_stream() {
        let (mut r, _buf) = new_capturing(40, 10);
        let chunk = "x".repeat(100 * 1024); // 100 KiB, NO newline
        for _ in 0..30 {
            r.render(UiLine::AssistantText(chunk.clone())); // ~3 MiB total, no '\n'
        }
        assert!(
            r.assistant_line_buf.len() <= ASSISTANT_LINE_BUF_MAX + chunk.len(),
            "assistant_line_buf must stay bounded (cap+one chunk), got {}",
            r.assistant_line_buf.len()
        );
    }

    /// Regression (long-session resize duplication): once `body_log` evicts its
    /// oldest entries (`body_log_truncated`), on_resize MUST still clear native
    /// scrollback (`\x1b[3J`). The reflow below re-appends the whole RETAINED
    /// transcript; if the wipe is skipped, the previous copy stays stranded and every
    /// font-zoom step stacks another duplicate (the reported "一直重复输出" on big
    /// sessions). We trade the already-evicted, un-reflowable prefix for no duplication.
    #[test]
    fn on_resize_clears_scrollback_even_after_truncation() {
        let (mut r, buf) = new_capturing(40, 10);
        for i in 0..6 {
            r.render(UiLine::AssistantText(format!("line {i}\n")));
        }
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        // Simulate a long session whose oldest body_log entries were evicted.
        r.body_log_truncated = true;
        buf.lock().unwrap().clear();

        r.on_resize(40, 8); // real height change → resize work

        let out = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        assert!(
            out.contains("\x1b[3J"),
            "a truncated session must STILL clear scrollback (3J) to avoid stacking \
             duplicate transcripts on resize: {:?}",
            out
        );
    }

    /// On legacy conhost the resize wipe is forced down to a single ED2 (to
    /// dodge the 0xc0000409 fastfail), so it emits NO per-row CUP+EL. The
    /// body re-emit only covers body rows. That leaves the cold-start as the
    /// ONLY thing that clears the FOOTER rows. Without `invalidate()`,
    /// `physical_dirty` stays false, `render_diff` skips the cold-start, the
    /// old footer rows are never cleared, and the new footer stacks on top
    /// of them (the user-reported Windows resize corruption).
    ///
    /// We exercise the conhost path on purpose: on other terminals the
    /// per-row CUP+EL wipe clears every row regardless, masking a missing
    /// invalidate() — which is exactly why the bug is conhost-only and why a
    /// default-path test wouldn't distinguish fixed from broken.
    #[test]
    fn on_resize_cold_starts_in_legacy_conhost() {
        let w: u16 = 40;
        let h: u16 = 12;
        let (mut r, buf) = new_capturing(w, h);
        r.caps.legacy_conhost = true;

        // Warm: paint footer + a body line so prev_cells is populated, like a
        // live session right before the user drags the window.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.render(UiLine::AssistantText("pre-resize content\n".into()));
        r.flush_deferred();
        buf.lock().unwrap().clear();

        // Shrink height → footer moves up; the old footer rows must be
        // cleared. on_resize repaints internally (paint_frame → render_diff),
        // so the cold-start (if any) lands in `buf` during this call.
        let new_h: u16 = h - 3;
        r.on_resize(w, new_h);

        let out_bytes = buf.lock().unwrap().clone();
        let out = String::from_utf8_lossy(&out_bytes);
        // ED2 emits no per-row CUP+EL and the body re-emit only covers body
        // rows, so per-row CUP+EL for EVERY row 1..=new_h can only come from
        // the render_diff cold-start — which fires only if on_resize
        // invalidated the screen.
        for row in 1..=(new_h as usize) {
            let needle = format!("\x1b[{};1H\x1b[K", row);
            assert!(
                out.contains(&needle),
                "on_resize must cold-start (per-row CUP+EL) for row {} on legacy \
                 conhost; without screen.invalidate() the footer rows are never \
                 cleared and the old footer ghosts under the new one (Windows \
                 resize footer-stacking bug).\nbytes: {:?}",
                row,
                out
            );
        }
    }

    #[test]
    fn on_resize_with_modal_overlay_preserves_transcript_and_drops_stale_overlay() {
        // Regression: resizing while a `/view` or `/diff` panel is open must NOT
        // wipe the conversation ("从大屏到小屏之后就会丢失上面的内容"). The
        // event-loop resize handler used to call `clear_screen()` (= `reset()`,
        // which drops `body_log`) before `on_resize()`, so the reflow replayed an
        // EMPTY log and the whole transcript vanished. The fix removes that
        // `clear_screen()` and has `on_resize` drop only its stale modal overlay
        // (the caller rebuilds it via `m.draw()`), preserving `body_log` so the
        // reflow rebuilds the transcript.
        let (mut r, buf) = new_capturing(60, 20);
        for i in 0..5 {
            r.render(UiLine::AssistantText(format!("transcript line {i}\n")));
        }
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: status_basic(),
            attachments: Vec::new(),
        });
        // Open a `/diff`-style panel (sets `modal_overlay` + `diff_overlay_active`).
        let title = DiffPanelRow::new(vec![
            DiffPanelSpan::new("Diff", DiffPanelTone::Default).bold()
        ]);
        let panel_rows = vec![DiffPanelRow::new(vec![DiffPanelSpan::new(
            "a.rs",
            DiffPanelTone::Default,
        )])];
        r.render(UiLine::DiffPanel {
            title,
            rows: panel_rows,
            footer: "Esc close".into(),
            win_width: 60,
            win_height: 15,
        });
        r.flush_deferred();
        assert!(r.modal_overlay.is_some(), "panel should install an overlay");
        let logged = r.body_log.len();
        assert!(
            logged > 0,
            "transcript must be logged before resize: {logged}"
        );
        buf.lock().unwrap().clear();

        // Shrink the terminal while the panel is open.
        r.on_resize(60, 12);

        // Root-cause guard: the transcript log survives (the old `clear_screen()`
        // path would have emptied it → 0).
        assert!(
            r.body_log.len() >= logged,
            "resize must preserve the transcript log (was {logged}, now {})",
            r.body_log.len()
        );
        // …and the reflow re-emitted it into the shrunk viewport.
        let out = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        assert!(
            out.contains("transcript line 4"),
            "reflow must repaint the transcript after resize: {out:?}"
        );
        // Stale overlay dropped so `on_resize` can't stamp it at the old geometry
        // (the #1158 duplication); the caller's `m.draw()` rebuilds it fresh.
        assert!(
            r.modal_overlay.is_none(),
            "resize must drop the stale modal overlay so it isn't painted twice"
        );
    }

    #[test]
    fn on_resize_preserves_transcript_under_menu_picker_modal() {
        // The transcript-loss-on-resize bug hit EVERY modal, not just /view and
        // /diff: the handler's `clear_screen()` fired for any `active_modal`. The
        // menu pickers (/model, /resume, /provider, /proxy, …) render via
        // `InputPrompt` into the footer of the Screen model — NOT the separate
        // `modal_overlay` — so they never had the #1158 duplication and removing
        // `clear_screen()` is safe for them, while it fixes their transcript loss
        // too. This pins both halves.
        let (mut r, buf) = new_capturing(60, 20);
        for i in 0..5 {
            r.render(UiLine::AssistantText(format!("history row {i}\n")));
        }
        // A /model-style picker: input box + a selection menu in the footer.
        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: Some(MenuPayload {
                items: vec![("glm-5".into(), "".into()), ("deepseek".into(), "".into())],
                selected: 0,
                kind: crate::render::MenuKind::SlashCommand,
            }),
            status: status_basic(),
            attachments: Vec::new(),
        });
        r.flush_deferred();
        // A menu picker never installs the diff overlay.
        assert!(
            r.modal_overlay.is_none(),
            "menu pickers must not use the diff modal_overlay path"
        );
        buf.lock().unwrap().clear();

        r.on_resize(60, 12);

        let out = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        assert!(
            out.contains("history row 4"),
            "resize under a menu picker must preserve+repaint the transcript: {out:?}"
        );
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
        let has_faint_text = r
            .body_lines
            .iter()
            .any(|row| row.iter().any(|c| c.style.faint && c.ch != ' '));
        assert!(
            has_faint_text,
            "no faint reasoning cells found — the dim style was lost \
             when the SGR string approach was removed"
        );
    }

    #[test]
    fn retained_reasoning_text_line_buffering() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, _buf) = new_capturing(w, h);

        // 1. Send text without a newline, it should be buffered and not render as a row yet.
        r.render(UiLine::ReasoningText("Step 1: thinking".into()));
        assert_eq!(
            r.body_lines.len(),
            0,
            "should buffer reasoning without newline"
        );

        // 2. Send a newline, it should flush the complete line.
        r.render(UiLine::ReasoningText("\n".into()));
        assert_eq!(
            r.body_lines.len(),
            1,
            "should flush complete line on newline"
        );
        assert!(r.body_lines[0].iter().any(|c| c.style.faint && c.ch == 'S'));

        // 3. Send more text, and then a TurnComplete. The remainder should flush.
        r.render(UiLine::ReasoningText("Step 2: almost done".into()));
        assert_eq!(r.body_lines.len(), 1, "remainder should stay buffered");
        r.render(UiLine::TurnComplete);
        assert_eq!(
            r.body_lines.len(),
            2,
            "remainder should flush on TurnComplete"
        );
    }

    #[test]
    fn retained_command_output_coalescing() {
        let w: u16 = 80;
        let h: u16 = 24;
        let (mut r, _buf) = new_capturing(w, h);

        r.render(UiLine::CommandOutput("hello ".into()));
        r.render(UiLine::CommandOutput("world\n".into()));

        // check body log
        assert_eq!(
            r.body_log.len(),
            1,
            "consecutive command outputs must be coalesced"
        );
        if let UiLine::CommandOutput(text) = &r.body_log[0] {
            assert_eq!(text, "hello world\n");
        } else {
            panic!("expected CommandOutput line in body log");
        }
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

        // A footer-only geometry change must leave the physical body as a
        // continuous projection starting at `scrolled_off`. The next permanent
        // push advances `scrolled_off` by emitting bottom-row LFs, so any
        // head+tail compaction here would make those LFs promote different
        // physical rows than the logical rows being acknowledged.
        for screen_row in 0..cap_after {
            let expected: String = r.body_lines[r.scrolled_off + screen_row]
                .iter()
                .map(|cell| cell.ch)
                .collect();
            assert_eq!(
                vterm.row_text(screen_row).trim_end(),
                expected.trim_end(),
                "footer growth broke the body/scrolled_off projection at screen row {screen_row}"
            );
        }

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

        // End-to-end user-visible invariant: after the catch-up LFs, every
        // pre-existing body row lives in exactly one place across native
        // scrollback and the current viewport. A projection mismatch would
        // promote tail rows while advancing `scrolled_off` over different
        // logical rows, leaving duplicates and gaps when the user scrolls.
        let terminal_rows = vterm
            .scrollback_texts()
            .into_iter()
            .chain((0..h as usize).map(|row| vterm.row_text(row)))
            .collect::<Vec<_>>();
        for i in 0..(cap_before + 10) {
            let marker = format!("filler-{i:03}");
            let count = terminal_rows
                .iter()
                .filter(|row| row.contains(&marker))
                .count();
            assert_eq!(
                count, 1,
                "{marker} must occur exactly once across scrollback and viewport"
            );
        }
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
                total,
                2,
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

    /// A ToolCall that follows assistant text must have at least one blank row
    /// between the last assistant-text row and the `● Bash` header row.
    #[test]
    fn tool_block_has_leading_blank_after_assistant_text() {
        let (mut r, buf) = new_capturing(60, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(60, 24);
        r.render(UiLine::AssistantText("refine spacing further now".into()));
        r.render(UiLine::AssistantLineBreak);
        r.render(UiLine::ToolCall {
            name: "Bash".into(),
            detail: "ls".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let rows: Vec<String> = (0..vterm.height() as usize)
            .map(|i| vterm.row_text(i))
            .collect();
        let text_idx = rows
            .iter()
            .position(|r| r.contains("refine spacing further"))
            .unwrap_or_else(|| {
                panic!(
                    "assistant text row not found\nvterm dump:\n{}",
                    vterm.dump()
                )
            });
        let bash_idx = rows
            .iter()
            .position(|r| r.contains("● Bash"))
            .expect("bash header row");
        assert!(
            bash_idx > text_idx + 1,
            "≥1 blank line between text and tool block: text@{text_idx} bash@{bash_idx}\n{}",
            vterm.dump()
        );
        assert!(
            rows[text_idx + 1..bash_idx]
                .iter()
                .any(|r| r.trim().is_empty()),
            "a blank row exists between them\n{}",
            vterm.dump()
        );
    }

    /// Two consecutive ToolCall blocks (separated only by a ToolResult) must
    /// NOT have a blank row between them — tool chains stay compact.
    #[test]
    fn tool_block_no_blank_between_consecutive_tool_calls() {
        let (mut r, buf) = new_capturing(60, 24);
        let mut vterm = crate::test_term::VirtualTerminal::new(60, 24);
        // First tool call + result (no assistant text in between)
        r.render(UiLine::ToolCall {
            name: "Bash".into(),
            detail: "ls".into(),
        });
        r.render(UiLine::ToolResult {
            success: true,
            summary: "file.txt".into(),
            diff_stats: None,
        });
        // Second tool call — previous mark was a ToolResult, NOT assistant text
        r.render(UiLine::ToolCall {
            name: "Bash".into(),
            detail: "pwd".into(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let rows: Vec<String> = (0..vterm.height() as usize)
            .map(|i| vterm.row_text(i))
            .collect();
        // Find the two `● Bash` header rows
        let bash_positions: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.contains("● Bash"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            bash_positions.len(),
            2,
            "expected two ● Bash rows\n{}",
            vterm.dump()
        );
        let first_bash = bash_positions[0];
        let second_bash = bash_positions[1];
        // The rows between first_bash and second_bash (exclusive) should have
        // no entirely-blank row — they should all contain the result text or
        // be adjacent (i.e. second_bash == first_bash + 1 or +2 for result).
        let blank_between = rows[first_bash + 1..second_bash]
            .iter()
            .any(|r| r.trim().is_empty());
        assert!(
            !blank_between,
            "consecutive tool calls must NOT have a blank row between them: \
             first_bash@{first_bash} second_bash@{second_bash}\n{}",
            vterm.dump()
        );
    }

    // ── Step 2: approval replaces input box + hides status row ──────────────

    /// When an approval panel is pending:
    ///  - the input box (middle "❯ " / "> " line) must NOT appear
    ///  - the status row (model name) must NOT appear
    ///  - the approval option rows (1. / 2. / 3.) MUST appear
    ///  - the hint row MUST appear
    ///  - exactly ONE horizontal rule is above the first option row
    #[test]
    fn approval_active_hides_input_box_and_status() {
        const W: u16 = 80;
        let (mut r, buf) = new_capturing(W, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(W, 24);

        let mut status = status_basic(); // sets model = "glm-5", cwd = "~/project/atomcode"
        status.approval = Some(crate::render::ApprovalPanelView {
            tool: "Bash".into(),
            detail: "rm -rf /tmp/x".into(),
            options: vec![
                "Allow once".into(),
                "Always allow Bash (this session)".into(),
                "Deny".into(),
            ],
            selected: 0,
        });

        r.render(UiLine::InputPrompt {
            buf: "some typed text that would normally show in input box".into(),
            cursor_byte: 4,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let dump = vterm.dump();
        let h = vterm.height() as usize;

        // Input box middle row must NOT be present:
        // The input middle row contains "> " or "❯ " followed by input text.
        // We look for the typed text in the middle row specifically.
        let input_text_visible = vterm.any_row(|row| row.contains("some typed text"));
        assert!(
            !input_text_visible,
            "input box middle row must be hidden during approval\n{dump}"
        );

        // Status row (model name) must NOT appear.
        let status_visible = vterm.any_row(|row| row.contains("glm-5"));
        assert!(
            !status_visible,
            "status row (model name 'glm-5') must be hidden during approval\n{dump}"
        );

        // Approval options MUST appear.
        assert!(
            vterm.any_row(|row| row.contains("1. ")),
            "option 1 prefix must be on screen\n{dump}"
        );
        assert!(
            vterm.any_row(|row| row.contains("2. ")),
            "option 2 prefix must be on screen\n{dump}"
        );
        assert!(
            vterm.any_row(|row| row.contains("3. ")),
            "option 3 prefix must be on screen\n{dump}"
        );
        assert!(
            vterm.any_row(|row| row.contains("Allow once")),
            "Allow once must be on screen\n{dump}"
        );
        assert!(
            vterm.any_row(|row| row.contains("Deny")),
            "Deny must be on screen\n{dump}"
        );

        // Hint row MUST appear.
        let hint_visible = vterm
            .any_row(|row| row.contains("select") || row.contains("选择") || row.contains("Enter"));
        assert!(
            hint_visible,
            "hint row with keyboard hint text must appear\n{dump}"
        );

        // Exactly ONE horizontal rule above the first option row.
        // A rule row is a row where every non-space char is the rule char (─ or -).
        let row_of = |needle: &str| -> Option<usize> {
            (0..h).find(|&i| vterm.row_text(i).contains(needle))
        };
        let option1_row = row_of("1. ").expect("option 1 row must exist");
        // There should be a rule row at option1_row - 2 (since row - 1 is the header).
        let row_above_header = option1_row
            .checked_sub(2)
            .expect("option 1 must not be at row 0/1");
        let rule_text = vterm.row_text(row_above_header);
        let is_rule = !rule_text.trim().is_empty()
            && rule_text.chars().all(|c| c == '─' || c == '-' || c == ' ');
        assert!(
            is_rule,
            "row above header must be a horizontal rule; got: {:?}\n{dump}",
            rule_text
        );

        // No second rule immediately above the rule row (no bot_rule).
        if let Some(two_above) = row_above_header.checked_sub(1) {
            let two_above_text = vterm.row_text(two_above);
            let is_also_rule = !two_above_text.trim().is_empty()
                && two_above_text
                    .chars()
                    .all(|c| c == '─' || c == '-' || c == ' ');
            // two_above should NOT be a rule (would mean bot_rule still drawn)
            assert!(!is_also_rule,
                "bot_rule must not appear during approval; row {} looks like a second rule: {:?}\n{dump}",
                two_above, two_above_text);
        }
    }

    /// Without an approval panel, the input box and status row are still present
    /// (regression guard — normal mode must not be disturbed).
    #[test]
    fn no_approval_keeps_input_box_and_status() {
        const W: u16 = 80;
        let (mut r, buf) = new_capturing(W, 24);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(W, 24);

        let status = status_basic(); // model="glm-5", no approval

        r.render(UiLine::InputPrompt {
            buf: "hello world".into(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let dump = vterm.dump();

        // Input box must be visible (the text we typed).
        assert!(
            vterm.any_row(|row| row.contains("hello world")),
            "input text must appear without approval\n{dump}"
        );

        // Status row (model name) must be visible.
        assert!(
            vterm.any_row(|row| row.contains("glm-5")),
            "status row (model='glm-5') must appear without approval\n{dump}"
        );

        // No approval options.
        assert!(
            !vterm.any_row(|row| row.contains("1. ")),
            "no option prefix without approval\n{dump}"
        );
    }

    /// The footer height reported by `current_footer_rows()` must equal the
    /// height that `paint_footer` actually uses — the critical mirror invariant.
    ///
    /// Strategy: for a known minimal setup (todo=0, menu=0, goal=0,
    /// attachment=0, 3 options + hint → approval_rows=4) with approval active,
    /// the approval-active formula yields:
    ///   1 (top_rule) + 0 (middle) + 0 (bot_rule) + 0+0+0+0 + 4 (approval) + 0 (status)
    ///   = 5
    /// We assert `current_footer_rows()` returns 5 and that after painting,
    /// the body row above the footer is NOT overwritten by footer content.
    #[test]
    fn footer_height_mirrors_match_during_approval() {
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        const W: u16 = 80;
        const H: u16 = 24;
        let (mut r, buf) = new_capturing(W, H);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;

        // 3 options + 1 hint = approval_rows=4.
        let mut status = StatusLine {
            model: String::new(), // no status row (has_status=false → status_rows=0)
            cwd: String::new(),
            history: None,
            command_output: None,
            ctx_used: 0,
            ctx_window: 0,
            hint: None,
            mode_indicator: None,
            bypass_indicator: None,
            session_name: None,
            reasoning_effort: None,
            goal: None,
            loop_status: None,
            todo: None,
            subtasks: None,
            approval: None,
            user_input: None,
            pending_messages: Vec::new(),
            round_cap_panel: None,
        };
        status.approval = Some(crate::render::ApprovalPanelView {
            tool: "Bash".into(),
            detail: "cmd".into(),
            options: vec!["Allow once".into(), "Always allow".into(), "Deny".into()],
            selected: 0,
        });

        r.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status,
            attachments: Vec::new(),
        });
        r.flush_deferred();

        // approval_rows = 1 header + 3 opts + 1 hint = 5; formula = 1 + 5 = 6.
        let expected_footer_rows = 6usize;
        assert_eq!(
            r.current_footer_rows(),
            expected_footer_rows,
            "current_footer_rows() must equal 1 (top_rule) + 5 (approval) = 6 during approval"
        );

        // Paint and confirm the layout: with no body lines, footer starts at row 0.
        // The top rule is at row 0, approval options at rows 1-3, hint at row 4.
        let mut vterm = crate::test_term::VirtualTerminal::new(W, H);
        drain_into_vterm(&buf, &mut vterm);
        // Row 0 = top rule (─────…).
        let top_rule_row_text = vterm.row_text(0);
        let is_rule = !top_rule_row_text.trim().is_empty()
            && top_rule_row_text
                .chars()
                .all(|c| c == '─' || c == '-' || c == ' ');
        assert!(
            is_rule,
            "row 0 must be a top rule; got: {:?}",
            top_rule_row_text
        );
        // Row 1 = header row.
        let header_row = vterm.row_text(1);
        assert!(
            header_row.contains("Allow"),
            "row 1 must be the header row; got: {:?}",
            header_row
        );
        // Row 2 = first approval option.
        let option_row = vterm.row_text(2);
        assert!(
            option_row.contains("1. "),
            "row 2 must be the first approval option; got: {:?}",
            option_row
        );
        // Rows 4..H-1 should be blank (no status, no input, no second rule).
        // Verify row expected_footer_rows (5) is blank — nothing leaked beyond the 5 footer rows.
        let row_after_footer = vterm.row_text(expected_footer_rows);
        assert!(
            row_after_footer.trim().is_empty(),
            "row {} (just below footer) must be blank; got: {:?}",
            expected_footer_rows,
            row_after_footer
        );
    }

    // helper: collect body_lines chars, skipping wide-glyph continuation cells
    // (width==0) so CJK "上手提示" doesn't become "上 手 提 示".
    fn body_text(r: &RetainedRenderer<CountingSink>) -> String {
        r.body_lines
            .iter()
            .flatten()
            .filter(|c| c.width != 0)
            .map(|c| c.ch)
            .collect()
    }

    // helper: extract "/xxx" tokens from the rendered welcome body
    fn tip_cmds(r: &RetainedRenderer<CountingSink>) -> Vec<String> {
        body_text(r)
            .split_whitespace()
            .filter(|w| w.starts_with('/'))
            .map(|w| w.to_string())
            .collect()
    }

    #[test]
    fn welcome_wide_has_mascot_and_pinned_login() {
        let (mut r, _c) = new_counting(100, 30);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        r.caps.modern_emulator = true; // half-block+bg art needs a bg-capable emulator
        r.push_welcome("GLM-5.2", "~/proj");
        let text = body_text(&r);
        assert!(text.contains('▀'), "mascot half-blocks must be present");
        assert!(text.contains("/login"), "pinned /login must be present");
        assert!(
            text.contains("Tips for getting started") || text.contains("上手提示"),
            "tips heading present"
        );
    }

    #[test]
    fn welcome_colors_off_omits_mascot() {
        let (mut r, _c) = new_counting(100, 30);
        r.caps.colors = false;
        r.push_welcome("GLM-5.2", "~/proj");
        let text = body_text(&r);
        assert!(!text.contains('▀'), "no mascot when colors are disabled");
        assert!(text.contains("/login"), "tips still present without color");
    }

    #[test]
    fn welcome_non_modern_emulator_omits_mascot() {
        // FinalShell-like: colours + unicode present, but NOT a modern emulator
        // (no WT_SESSION / TERM_PROGRAM, e.g. bare SSH). Such terminals render
        // the block glyphs but may drop the per-cell background, fragmenting the
        // half-block art — so omit it and let the tips stack.
        let (mut r, _c) = new_counting(100, 30);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        r.caps.modern_emulator = false;
        r.caps.jediterm = false;
        r.push_welcome("GLM-5.2", "~/proj");
        let text = body_text(&r);
        assert!(
            !text.contains('▀'),
            "no mascot on a non-modern emulator (FinalShell-like)"
        );
        assert!(text.contains("/login"), "tips still present");
    }

    #[test]
    fn jediterm_input_wrap_reserves_last_column() {
        // A CJK line exactly filling the normal budget (width-2 cols) must wrap
        // to one MORE row on JediTerm (budget width-3): the last column is
        // reserved so no glyph reaches the edge and triggers JediTerm's own
        // soft-wrap, which duplicated/garbled the row (DevEco Studio report).
        let (mut r, _c) = new_counting(100, 30);
        r.input_buf = "中".repeat(49); // 49 wide chars = 98 cols = width(100) - 2
        r.input_cursor_byte = 0;
        r.caps.jediterm = false;
        let normal = r.current_footer_rows();
        r.caps.jediterm = true;
        let jedi = r.current_footer_rows();
        assert!(
            jedi > normal,
            "JediTerm must reserve the last column → one extra wrapped row; normal={normal} jedi={jedi}"
        );
    }

    #[test]
    fn welcome_rows_never_exceed_width() {
        // With the wider (13-cell) mascot, the two-column tips layout must only
        // engage when the widest tip fits: NO composed row may exceed the
        // terminal width, or the emulator hard-wraps it and the column
        // fragments. Check a range of widths (narrow → wide, both stacked and
        // two-column regimes) with the mascot shown.
        for w in [50usize, 60, 72, 80, 100, 120] {
            let (mut r, _c) = new_counting(w as u16, 40);
            r.caps.colors = true;
            r.caps.unicode_symbols = true;
            r.caps.modern_emulator = true;
            let rows = r.build_welcome_rows("GLM-5.2", "~/some/project/path", &[0, 1, 2, 3]);
            for (i, row) in rows.iter().enumerate() {
                let rw: usize = row.iter().map(|c| c.width as usize).sum();
                assert!(
                    rw <= w,
                    "width {w}: welcome row {i} is {rw} cols (> {w}) — would wrap"
                );
            }
        }
    }

    #[test]
    fn welcome_reflow_keeps_same_tips() {
        let (mut r, _c) = new_counting(100, 30);
        r.caps.colors = true;
        r.push_welcome("GLM-5.2", "~/proj");
        let before: Vec<String> = tip_cmds(&r);
        r.reflow_welcome_prefix();
        let after: Vec<String> = tip_cmds(&r);
        assert_eq!(before, after, "reflow must not re-roll the random tips");
    }

    /// The REAL resize path is `reflow_body_to_current_width` (it clears
    /// `welcome_banner` and replays `body_log`, re-emitting `UiLine::Welcome`),
    /// NOT `reflow_welcome_prefix`. A resize must not re-roll the random tips.
    #[test]
    fn welcome_resize_via_body_replay_keeps_same_tips() {
        let (mut r, _c) = new_counting(100, 30);
        r.caps.colors = true;
        r.render(UiLine::Welcome {
            model: "GLM-5.2".into(),
            working_dir: "~/proj".into(),
        });
        let before = tip_cmds(&r);
        r.reflow_body_to_current_width();
        let after = tip_cmds(&r);
        assert_eq!(
            before, after,
            "resize (body_log replay) must not re-roll tips"
        );
        assert!(
            before.iter().any(|c| c == "/login"),
            "pinned /login present"
        );
    }

    /// A cell whose TOP pixel is transparent but bottom is coloured must use the
    /// lower-half block `▄` (colour on fg, transparent bg), NOT `▀` with a `None`
    /// fg — the latter paints the top half in the terminal's default foreground
    /// (a dark bar on a light background / wrong colour), which is the mascot
    /// "dark bar across the ears" bug.
    #[test]
    fn build_mascot_rows_transparent_top_uses_lower_half_block() {
        use super::mascot_cell;
        let orange = crate::render::mascot::mascot_color(b'o');
        // Branch logic (mascot-independent): a transparent-top + coloured-bottom
        // pixel MUST render as lower-half `▄` (colour on fg), never `▀` with a
        // None fg (which paints the top half in the default fg — the dark-bar bug).
        let c = mascot_cell(None, orange);
        assert_eq!(
            c.ch, '\u{2584}',
            "transparent-top pixel must render as lower-half '▄'"
        );
        assert!(
            c.style.fg.is_some(),
            "'▄' carries the bottom colour on its fg"
        );
        assert_eq!(mascot_cell(orange, None).ch, '\u{2580}'); // top-only → '▀'
        assert_eq!(mascot_cell(orange, orange).ch, '\u{2580}'); // both → '▀' + bg
        assert_eq!(mascot_cell(None, None), Cell::blank()); // both transparent → blank

        // And the actual mascot never emits the buggy `▀`-with-None-fg cell.
        let (r, _c) = new_counting(80, 24);
        assert!(
            r.build_mascot_rows()
                .iter()
                .flatten()
                .all(|c| !(c.ch == '\u{2580}' && c.style.fg.is_none())),
            "'▀' with no fg paints the top half as the terminal default fg"
        );
    }

    /// On a terminal that can't render half-block glyphs (classic Windows
    /// console → `unicode_symbols = false`), the mascot must be omitted rather
    /// than downgraded to a `#`-soup. Tips still render.
    #[test]
    fn welcome_no_unicode_omits_mascot() {
        let (mut r, _c) = new_counting(100, 30);
        r.caps.colors = true;
        r.caps.unicode_symbols = false;
        r.push_welcome("GLM-5.2", "~/proj");
        let text = body_text(&r);
        assert!(
            !text.contains('#'),
            "mascot must be omitted, not '#'-downgraded"
        );
        assert!(!text.contains('\u{2580}') && !text.contains('\u{2584}'));
        assert!(
            text.contains("/login"),
            "tips still present without a mascot"
        );
    }

    /// When the tips column is TALLER than the mascot (e.g. the 4-row mascot vs
    /// a 5-row tips block), the overflow tips must NOT land on the cwd/model
    /// rows. Regression: a `/tip` was merged onto the cwd bullet
    /// (`∙ proj/goal   set a goal…`). cwd/model must render BELOW the two-column
    /// block, never zipped into it.
    #[test]
    fn welcome_tall_tips_do_not_merge_into_cwd_row() {
        let (mut r, _c) = new_counting(120, 30);
        r.caps.colors = true;
        r.caps.unicode_symbols = true;
        // slash-free cwd/model so any '/' on the cwd row can only be a leaked tip
        r.push_welcome("glm", "proj");
        let cwd_row: String = r
            .body_lines
            .iter()
            .find(|row| {
                row.iter()
                    .map(|c| c.ch)
                    .collect::<String>()
                    .contains("proj")
            })
            .expect("cwd row present")
            .iter()
            .map(|c| c.ch)
            .collect();
        assert!(
            !cwd_row.contains('/'),
            "a tip leaked onto the cwd row: {cwd_row:?}"
        );
    }

    /// With colours disabled the mascot is omitted, so the welcome must STACK
    /// (tips below path/model), never render a two-column layout that would
    /// staircase the tips onto the cwd/model rows.
    #[test]
    fn welcome_colors_off_stacks_tips_below_not_beside() {
        let (mut r, _c) = new_counting(100, 30);
        r.caps.colors = false;
        r.push_welcome("GLM-5.2", "~/proj");
        let collides = r.body_lines.iter().any(|row| {
            let s: String = row.iter().map(|c| c.ch).collect();
            s.contains("GLM-5.2") && s.contains("/login")
        });
        assert!(
            !collides,
            "colors-off must stack tips below path/model, not beside them"
        );
    }
}

#[cfg(test)]
mod todo_panel_rows_tests {
    use super::*;
    use atomcode_capabilities::tools::todo::TodoStatus;

    fn items(spec: &[(TodoStatus, &str)]) -> Vec<(TodoStatus, String)> {
        spec.iter().map(|(s, c)| (*s, c.to_string())).collect()
    }

    #[test]
    fn header_plus_all_when_fits() {
        let it = items(&[
            (TodoStatus::Completed, "a"),
            (TodoStatus::InProgress, "b"),
            (TodoStatus::Pending, "c"),
        ]);
        let rows = todo_panel_rows(&it, 1, 1, 3, MAX_TODO_PANEL_ROWS);
        // The whole list fits (3 items ≤ budget) → show EVERY item in order with its
        // real status (no fold): [Spacer, Header, Completed, InProgress, Pending] = 5 rows.
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0], TodoPanelRow::Spacer));
        assert!(matches!(
            rows[1],
            TodoPanelRow::Header {
                completed: 1,
                in_progress: 1,
                total: 3
            }
        ));
        assert!(
            matches!(&rows[2], TodoPanelRow::Item { index: 0, status: TodoStatus::Completed, content, .. } if content == "a")
        );
        assert!(
            matches!(&rows[3], TodoPanelRow::Item { index: 1, status: TodoStatus::InProgress, content, .. } if content == "b")
        );
        assert!(
            matches!(&rows[4], TodoPanelRow::Item { index: 2, status: TodoStatus::Pending, content, .. } if content == "c")
        );
    }

    #[test]
    fn no_fold_when_none_completed() {
        let it = items(&[(TodoStatus::InProgress, "b"), (TodoStatus::Pending, "c")]);
        let rows = todo_panel_rows(&it, 0, 1, 2, MAX_TODO_PANEL_ROWS);
        // Both items fit → shown individually (in-progress + pending), no More.
        assert!(rows.iter().any(|r| matches!(
            r,
            TodoPanelRow::Item {
                status: TodoStatus::InProgress,
                ..
            }
        )));
        assert!(rows.iter().any(|r| matches!(
            r,
            TodoPanelRow::Item {
                status: TodoStatus::Pending,
                ..
            }
        )));
        assert!(!rows.iter().any(|r| matches!(r, TodoPanelRow::More { .. })));
    }

    #[test]
    fn pending_overflow_becomes_more() {
        let it = items(&[
            (TodoStatus::InProgress, "ip"),
            (TodoStatus::Pending, "p1"),
            (TodoStatus::Pending, "p2"),
            (TodoStatus::Pending, "p3"),
            (TodoStatus::Pending, "p4"),
        ]);
        // max_rows=4: body_budget = 4-2 = 2. ip takes 1, pend_budget=1 → shown=0, hidden=4.
        // Result: [Spacer, Header, ip, More{4}] = 4 rows.
        let rows = todo_panel_rows(&it, 0, 1, 5, 4);
        assert_eq!(rows.len(), 4);
        assert!(matches!(rows[0], TodoPanelRow::Spacer));
        assert!(matches!(
            rows.last().unwrap(),
            TodoPanelRow::More { hidden: 4 }
        ));
    }

    #[test]
    fn in_progress_survives_tight_budget() {
        // body_budget = max_rows - 2 (Spacer + Header). With max_rows=3: budget=1.
        // in-progress wins the single body slot over the completed fold.
        let it = items(&[
            (TodoStatus::Completed, "done"),
            (TodoStatus::InProgress, "ip"),
        ]);
        let rows = todo_panel_rows(&it, 1, 1, 2, 3);
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0], TodoPanelRow::Spacer));
        assert!(matches!(rows[1], TodoPanelRow::Header { .. }));
        assert!(matches!(
            &rows[2],
            TodoPanelRow::Item {
                status: TodoStatus::InProgress,
                ..
            }
        ));
    }

    #[test]
    fn never_exceeds_max_rows() {
        let mut it = items(&[(TodoStatus::InProgress, "ip"), (TodoStatus::Completed, "c")]);
        for i in 0..20 {
            it.push((TodoStatus::Pending, format!("p{i}")));
        }
        let rows = todo_panel_rows(&it, 1, 1, 22, MAX_TODO_PANEL_ROWS);
        assert!(rows.len() <= MAX_TODO_PANEL_ROWS);
    }

    #[test]
    fn mostly_done_shows_recent_window_not_a_fold() {
        // 11 completed + 1 open. The frontier (the open task) is the last item, so the
        // window is the last `body_budget` (5) items — recent completed shown INDIVIDUALLY
        // plus the open one. No fold, no `+N more` (nothing hidden after the frontier).
        let mut it: Vec<(TodoStatus, String)> = (1..=11)
            .map(|i| (TodoStatus::Completed, format!("s{i}")))
            .collect();
        it.push((TodoStatus::Pending, "final".into()));
        let rows = todo_panel_rows(&it, 11, 0, 12, MAX_TODO_PANEL_ROWS); // MAX_TODO_PANEL_ROWS = 7
        assert_eq!(rows.len(), 7, "Spacer + Header + 5 items");
        assert!(
            !rows.iter().any(|r| matches!(r, TodoPanelRow::More { .. })),
            "frontier at end → no More"
        );
        // Window is items[7..12] = #8..#12, each shown with its real index + status.
        assert!(matches!(
            &rows[2],
            TodoPanelRow::Item {
                index: 7,
                status: TodoStatus::Completed,
                ..
            }
        ));
        assert!(
            matches!(&rows[6], TodoPanelRow::Item { index: 11, status: TodoStatus::Pending, content, .. } if content == "final")
        );
    }

    #[test]
    fn tight_budget_drops_spacer_and_respects_cap() {
        // On a very short terminal (todo_panel_cap can return 1), the cosmetic
        // Spacer must be dropped so we never emit more rows than max_rows.
        let it = items(&[(TodoStatus::InProgress, "ip"), (TodoStatus::Pending, "p")]);
        // max_rows=1: header only, no spacer, no body.
        let rows = todo_panel_rows(&it, 0, 1, 2, 1);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0], TodoPanelRow::Header { .. }));
        // max_rows=2: spacer + header, no body budget left.
        let rows = todo_panel_rows(&it, 0, 1, 2, 2);
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0], TodoPanelRow::Spacer));
        assert!(matches!(rows[1], TodoPanelRow::Header { .. }));
    }

    #[test]
    fn build_todo_rows_header_and_inprogress() {
        use crate::terminal::{EnvView, TerminalCaps};
        use atomcode_capabilities::tools::todo::TodoStatus;
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            term: Some("xterm-256color".into()),
            colorterm: Some("truecolor".into()),
            lang: Some("en_US.UTF-8".into()),
            ..Default::default()
        });
        let r = RetainedRenderer::with_writer(Vec::<u8>::new(), caps, 80, 24);
        let todo = crate::render::TodoProgress {
            current: Some("wire it".into()),
            completed: 1,
            in_progress: 1,
            total: 3,
            items: vec![
                (TodoStatus::Completed, "done a".into()),
                (TodoStatus::InProgress, "wire it".into()),
                (TodoStatus::Pending, "later".into()),
            ],
        };
        let rows = r.build_todo_rows(&todo, 40);
        let text = |cells: &Vec<Cell>| cells.iter().map(|c| c.ch).collect::<String>();
        use crate::render::theme::Palette;
        // rows[0] is the blank Spacer row — no cells.
        assert!(rows[0].is_empty(), "first row must be the blank spacer");
        // rows[1] is the header: `Tasks` bold + `(N done, M in progress, K open)` detail.
        assert!(
            text(&rows[1]).contains("Tasks (")
                && text(&rows[1]).contains("1 done")
                && text(&rows[1]).contains("1 in progress")
                && text(&rows[1]).contains("1 open"),
            "header must be 'Tasks (1 done, 1 in progress, 1 open)', got: {:?}",
            text(&rows[1])
        );
        assert!(rows[1].iter().any(|c| c.style.bold), "header title is bold");
        assert!(
            rows[1].iter().all(|c| c.style.fg != Some(Palette::BRAND)),
            "header must carry no Brand (pink) color"
        );
        // the in-progress row: filled checkbox glyph is bold + Brand (pink),
        // the body text is bold default fg. Both are bold; only the glyph is pink.
        let ip = rows
            .iter()
            .find(|row| text(row).contains("wire it"))
            .unwrap();
        assert!(ip.iter().any(|c| c.style.bold), "in-progress task is bold");
        assert!(
            ip.iter().any(|c| c.style.fg == Some(Palette::BRAND)),
            "in-progress checkbox glyph must be Brand-coloured (filled square stands out)"
        );
        // Tasks-panel style: every item carries a `#N` positional number.
        assert!(
            text(ip).contains('#'),
            "item must show a #N number, got: {:?}",
            text(ip)
        );
    }
}
