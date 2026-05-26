//! Shared text-selection module used by both AltScreenRenderer and
//! RetainedRenderer. Owns: anchor/head pos, drag tracking, range
//! computation, line rendering with reverse-video highlight, OSC 52
//! emission and arboard fallback for Ctrl+C copy.
//!
//! Each renderer holds a `SelectionState` and implements `BodyLineView`
//! over its native body buffer type (`Vec<String>` for alt-screen,
//! `Vec<Vec<Cell>>` for retained).

use std::borrow::Cow;
use std::io::Write;
use unicode_segmentation::UnicodeSegmentation;

use crate::render::cell::Cell;
use crate::width::cluster_width;

/// A single (row, col) cursor position in body_lines coordinates.
/// `row` is the index into body_lines; `col` is display-column.
pub type BodyPos = (usize, u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: BodyPos,
    pub head: BodyPos,
}

#[derive(Debug, Default)]
pub struct SelectionState {
    pub selection: Option<Selection>,
    pub active: bool, // true while mouse button held down
}

/// Trait adapter so the selection module can read body content without
/// caring whether the renderer stores `Vec<String>` or `Vec<Vec<Cell>>`.
pub trait BodyLineView {
    fn line_count(&self) -> usize;
    fn line_text(&self, idx: usize) -> Cow<'_, str>;
}

// Impl for the alt-screen body_lines type.
impl BodyLineView for Vec<String> {
    fn line_count(&self) -> usize {
        self.len()
    }
    fn line_text(&self, idx: usize) -> Cow<'_, str> {
        Cow::Borrowed(self.get(idx).map(|s| s.as_str()).unwrap_or(""))
    }
}

// Impl for the retained body_lines type.
impl BodyLineView for Vec<Vec<Cell>> {
    fn line_count(&self) -> usize {
        self.len()
    }
    fn line_text(&self, idx: usize) -> Cow<'_, str> {
        let Some(row) = self.get(idx) else {
            return Cow::Borrowed("");
        };
        // Build a visible-text string from cells; skip continuation cells
        // (width == 0) which are placeholders for the 2nd column of a wide glyph.
        let s: String = row.iter().filter(|c| c.width > 0).map(|c| c.ch).collect();
        Cow::Owned(s)
    }
}

// ── SGR-aware text helpers ────────────────────────────────────────────

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
pub fn truncate_to_width_sgr_aware(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut acc = String::with_capacity(s.len());
    let mut cols = 0usize;
    // Byte-cursor walk so CSI escapes (multi-byte but each ASCII-char
    // grapheme) can be slurped as one zero-width unit while non-SGR
    // content advances grapheme-by-grapheme for cluster_width accuracy.
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
        if cols + w > max_cols {
            break;
        }
        acc.push_str(g);
        cols += w;
        i = next;
    }
    acc
}

/// Walk `s` and return the visible-text display width, treating CSI
/// escape sequences as zero-width spans (same parser as
/// `truncate_to_width_sgr_aware`). Used to clamp selection columns
/// against the actual painted content of a body line — clicks past the
/// end of the visible row should select nothing in the gap, not extend
/// to the column the user happened to drop on.
pub fn line_display_width_sgr_aware(s: &str) -> usize {
    let mut cols = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if bytes[i] == 0x1b && i + 1 < s.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < s.len() {
                let c = bytes[i];
                i += 1;
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let next = s[i..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(idx, _)| idx + i)
            .unwrap_or(s.len());
        cols += cluster_width(&s[i..next]);
        i = next;
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
/// Wide chars (CJK, emoji): a cluster is selected iff its visual
/// footprint OVERLAPS `[sel_start, sel_end)`. Concretely:
/// `cols + w > sel_start && cols < sel_end`. This matches what the
/// user expects when they drag-select any column inside a wide cluster
/// — the whole cluster joins the selection. The previous rule
/// `cols >= sel_start && cols < sel_end` silently dropped any cluster
/// whose first column was below `sel_start`, even when the cluster's
/// second column sat inside the selection.
pub fn render_line_with_selection(
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
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < line.len() {
        if bytes[i] == 0x1b && i + 1 < line.len() && bytes[i + 1] == b'[' {
            // Capture the full CSI span first so we can decide whether
            // to drop it (inside selection) or keep it (outside).
            let start = i;
            i += 2;
            while i < line.len() {
                let c = bytes[i];
                i += 1;
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            if !in_sel {
                out.push_str(&line[start..i]);
            }
            continue;
        }
        if cols >= max_cols {
            break;
        }
        let next = line[i..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(idx, _)| idx + i)
            .unwrap_or(line.len());
        let g = &line[i..next];
        let w = cluster_width(g);
        // Overlap test: the cluster spans `cols..cols+w`. It enters the
        // selection if any of those cols falls in [sel_start, sel_end).
        // For width-1 clusters this collapses to `cols >= sel_start &&
        // cols < sel_end` (same as before). For wide clusters whose
        // first col is below sel_start but second col is inside, the
        // cluster now joins the selection — see doc-comment for why.
        let want_in_sel = cols + w > sel_start && cols < sel_end;
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
        out.push_str(g);
        cols += w;
        i = next;
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
pub fn extract_line_selection_text(line: &str, sel_start: usize, sel_end: usize) -> String {
    if sel_end <= sel_start {
        return String::new();
    }
    let mut out = String::new();
    let mut cols = 0usize;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < line.len() {
        if bytes[i] == 0x1b && i + 1 < line.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < line.len() {
                let c = bytes[i];
                i += 1;
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if cols >= sel_end {
            break;
        }
        let next = line[i..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(idx, _)| idx + i)
            .unwrap_or(line.len());
        let g = &line[i..next];
        let w = cluster_width(g);
        // Match `render_line_with_selection`'s overlap rule so the
        // highlight and the copied text agree on which wide clusters
        // belong to the selection. A cluster overlapping sel_start
        // joins the copy; one overlapping sel_end falls in via the
        // `cols < sel_end` check (the break above covers >= sel_end).
        if cols + w > sel_start {
            out.push_str(g);
        }
        cols += w;
        i = next;
    }
    out
}

/// Compute which columns `[start, end)` of `line` lie inside the
/// selection `[lo, hi]` (inclusive line indices), clamped to the line's
/// visible display width. Returns `None` if the line is outside the
/// selection or the clamped range is empty.
///
/// Line indices in `lo`/`hi` are `(row, col)` pairs where `row` indexes
/// into body_lines and `col` is a display-column. End columns are
/// inclusive (the cell under the head is included in the selection).
///
/// Free function (rather than a method) so the body-paint loop can
/// call it while holding a borrow of `self.body_lines[i]` without
/// re-borrowing `self`.
pub fn selection_col_range_for_line(
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

// ── SelectionState methods ────────────────────────────────────────────

impl SelectionState {
    /// Start a new selection at body coordinates `pos`.
    pub fn begin(&mut self, pos: BodyPos) {
        self.selection = Some(Selection {
            anchor: pos,
            head: pos,
        });
        self.active = true;
    }

    /// Extend selection head to `pos` while button held.
    pub fn update(&mut self, pos: BodyPos) {
        if !self.active {
            return;
        }
        if let Some(sel) = self.selection.as_mut() {
            sel.head = pos;
        }
    }

    /// Finalise selection. Returns the selected text if non-empty, so the
    /// caller can emit OSC 52 to the host terminal. Selection state is
    /// preserved so the highlight stays drawn until the next click.
    /// Does NOT call emit_osc52 — that's the caller's responsibility.
    pub fn end<B: BodyLineView>(&mut self, body: &B) -> Option<String> {
        self.active = false;
        let sel = self.selection.as_ref()?;
        let text = extract_text(body, sel);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Copy current selection to system clipboard via arboard. Returns
    /// true iff a non-empty selection was copied. Clears highlight.
    pub fn copy<B: BodyLineView>(&mut self, body: &B) -> bool {
        let Some(sel) = self.selection else {
            return false;
        };
        let text = extract_text(body, &sel);
        if text.is_empty() {
            return false;
        }
        let copied = match arboard::Clipboard::new() {
            Ok(mut cb) => cb.set_text(text).is_ok(),
            Err(_) => false,
        };
        if copied {
            self.selection = None;
            self.active = false;
        }
        copied
    }

    pub fn clear(&mut self) {
        self.selection = None;
        self.active = false;
    }
}

/// Concatenate the selected text across (possibly multiple) body lines,
/// using the existing per-line range helpers.
fn extract_text<B: BodyLineView>(body: &B, sel: &Selection) -> String {
    let (lo, hi) = ord(sel.anchor, sel.head);
    let lo_us = (lo.0, lo.1 as usize);
    let hi_us = (hi.0, hi.1 as usize);
    let mut out = String::new();
    for row in lo.0..=hi.0 {
        let line = body.line_text(row);
        let Some((start, end)) = selection_col_range_for_line(row, lo_us, hi_us, &line) else {
            continue;
        };
        if row > lo.0 {
            out.push('\n');
        }
        out.push_str(&extract_line_selection_text(&line, start, end));
    }
    out
}

fn ord(a: BodyPos, b: BodyPos) -> (BodyPos, BodyPos) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

// ── Base64 + OSC 52 ───────────────────────────────────────────────────

/// Standard-alphabet base64 encoder. Inline implementation (~30 lines)
/// instead of pulling in the `base64` crate just for OSC 52: the
/// payload is one user-selected text blob per drag-release, kilobytes
/// at most, and the alphabet is fixed.
pub fn base64_encode(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

/// Emit OSC 52 (`\x1b]52;c;<base64>\x1b\\`) carrying `text` so the
/// host terminal copies it to the system clipboard. Empty text is
/// a no-op to avoid clearing whatever the user previously had.
/// Best-effort — terminals that don't honour OSC 52 (Terminal.app
/// without explicit opt-in) silently ignore the sequence.
///
/// Terminates with **String Terminator (ST = `\x1b\\`)** rather than
/// **BEL (`\x07`)** even though both are valid OSC terminators per
/// xterm. Some terminal emulators (notably classic conhost and a
/// handful of less-common Linux terminals — anything that maps BEL to
/// "play the audible/visible bell" even inside an OSC envelope) ring
/// the system bell every time an OSC 52 lands. The "Ctrl+C-rings-bell-
/// on-copy" symptom traces back to that: arboard-failure paths fell
/// through to `emit_osc52` and the BEL terminator was misinterpreted.
/// ST is the formally-defined ANSI terminator and is honoured silently
/// by every emulator we care about.
///
/// Takes `out: &mut dyn Write` rather than `&mut self` so both
/// AltScreenRenderer and RetainedRenderer can call it without
/// needing a shared struct.
pub fn emit_osc52(out: &mut dyn Write, text: &str) {
    if text.is_empty() {
        return;
    }
    let encoded = base64_encode(text.as_bytes());
    let _ = write!(out, "\x1b]52;c;{}\x1b\\", encoded);
    let _ = out.flush();
}

/// Copy `text` to the system clipboard. Tries arboard first (writes
/// directly to NSPasteboard / X11 / Win32 clipboard, doesn't touch the
/// terminal, doesn't trigger iTerm2's "may access clipboard" prompt).
/// Falls back to OSC 52 only if arboard fails — covers SSH sessions and
/// other contexts with no local clipboard service.
///
/// Empty text is a no-op (don't clobber whatever the user previously had).
pub fn copy_to_clipboard(out: &mut dyn Write, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Ok(mut cb) = arboard::Clipboard::new() {
        if cb.set_text(text.to_string()).is_ok() {
            return; // arboard succeeded; no need for OSC 52
        }
    }
    // arboard unavailable (headless / SSH / no clipboard service):
    // fall back to OSC 52 so a remote local terminal can still copy.
    emit_osc52(out, text);
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            out.starts_with("\x1b[0m\x1b[7m"),
            "should open with reset+reverse. got: {:?}",
            out
        );
        assert!(
            out.contains("hello"),
            "selected text missing. got: {:?}",
            out
        );
        assert!(
            out.contains("\x1b[0m world"),
            "post-selection plain text missing. got: {:?}",
            out
        );
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
        assert_eq!(
            resets, 2,
            "expected open-reset + close-reset only. got: {:?}",
            out
        );
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

    #[test]
    fn selection_state_begin_sets_anchor_and_active() {
        let mut s = SelectionState::default();
        s.begin((2, 5));
        assert_eq!(
            s.selection,
            Some(Selection {
                anchor: (2, 5),
                head: (2, 5)
            })
        );
        assert!(s.active);
    }

    #[test]
    fn selection_state_update_only_while_active() {
        let mut s = SelectionState::default();
        s.begin((0, 0));
        s.update((1, 4));
        assert_eq!(s.selection.unwrap().head, (1, 4));
        s.active = false;
        s.update((2, 9));
        // head shouldn't change after active = false
        assert_eq!(s.selection.unwrap().head, (1, 4));
    }

    #[test]
    fn selection_state_end_returns_concatenated_text() {
        let body: Vec<String> = vec!["first".into(), "second".into(), "third".into()];
        let mut s = SelectionState::default();
        s.begin((0, 3));
        s.update((2, 2));
        let text = s.end(&body).expect("non-empty");
        // Selection spans (0,3) → (2,2)
        assert_eq!(text, "st\nsecond\nthi");
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

    /// `BodyLineView` impl for `Vec<Vec<Cell>>` extracts visible text,
    /// skipping continuation cells (width == 0). Wide characters (e.g. CJK)
    /// take 2 columns; the 2nd column is represented by a continuation cell
    /// with width == 0. Only cells with width > 0 contribute to visible text.
    #[test]
    fn vec_vec_cell_line_text_extracts_visible_chars() {
        use crate::render::cell::CellStyle;

        let row = vec![
            Cell {
                ch: 'h',
                style: CellStyle::default(),
                width: 1,
            },
            Cell {
                ch: 'i',
                style: CellStyle::default(),
                width: 1,
            },
            Cell {
                ch: '中',
                style: CellStyle::default(),
                width: 2,
            },
            Cell {
                ch: ' ',
                style: CellStyle::default(),
                width: 0,
            }, // continuation
        ];
        let body: Vec<Vec<Cell>> = vec![row];
        assert_eq!(body.line_text(0), "hi中");
    }

    /// `copy_to_clipboard` with empty text must not emit OSC 52 or
    /// anything else — emitting a blank OSC 52 would clobber whatever
    /// the user previously had in their clipboard with an empty string.
    #[test]
    fn copy_to_clipboard_empty_is_noop() {
        let mut buf = Vec::new();
        copy_to_clipboard(&mut buf, "");
        assert!(
            buf.is_empty(),
            "empty text must not emit OSC 52 or anything else"
        );
    }

    /// `emit_osc52` MUST terminate with ST (`\x1b\\`), NOT BEL
    /// (`\x07`). Regression for the "selection copy rings system bell"
    /// bug — see the `emit_osc52` doc comment for the rationale.
    #[test]
    fn emit_osc52_terminates_with_st_not_bel() {
        let mut buf = Vec::new();
        emit_osc52(&mut buf, "hello");
        let s = std::str::from_utf8(&buf).expect("OSC 52 emit is ASCII");
        assert!(
            s.starts_with("\x1b]52;c;"),
            "OSC 52 must open with `\\x1b]52;c;`, got: {:?}",
            s
        );
        assert!(
            s.ends_with("\x1b\\"),
            "OSC 52 must terminate with ST (`\\x1b\\\\`), got: {:?}",
            s
        );
        assert!(
            !buf.contains(&b'\x07'),
            "OSC 52 must NOT contain BEL (`\\x07`) — some terminals \
             interpret it as 'ring the bell' even inside an OSC envelope; \
             got: {:?}",
            s
        );
    }

    // Note: we can't directly assert that arboard was called (would need
    // dependency injection), but we CAN assert that for non-empty text on
    // a successful arboard write, nothing reaches `out`. On a CI/headless
    // env where arboard fails, the OSC 52 fallback fires. Either is correct
    // behavior; we don't lock either path in here. Just verify the no-op guard.
}
