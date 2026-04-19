// crates/atomcode-tuix/src/render/cell.rs
//
// Ink-style cell buffer for footer/menu rendering.
//
// The row-level diff we had before was correct but coarse: any byte change
// in a row triggered a full-row re-emit. Combined with UTF-8 rule characters
// (`─` is 3 bytes × 200 cols × 2 rules = 1254 bytes of rule per redraw) and
// footer-height oscillation when the slash palette opens/closes, every
// menu toggle pushed 1800+ bytes to Mac Terminal.app's GUI pipeline — the
// threshold where its coalesce + repaint latency becomes user-visible.
//
// Ink (Claude Code's renderer) works on cells: (char, style) pairs indexed
// by absolute terminal position. New frame → diff cell-by-cell → emit
// minimal patches. A row whose status stayed "glm-5 · ~/project" across
// frames contributes zero bytes. Rule middles stay identical after a
// single-column input change → zero bytes. This module gives us that
// primitive.
//
// Scope: footer + slash palette only. Body content (streaming text, tool
// output) keeps the pure-append path — body lines enter scrollback and
// never need a diff cycle.

use crossterm::style::{Color, SetForegroundColor};
use std::collections::HashMap;
use std::io::Write as _;

/// Visual attributes that can vary per cell in our footer. Kept minimal
/// on purpose: footer uses fg color, bold, and reverse-video
/// (for the palette's selected row). Extending this to bg / underline
/// / italic is a future concern — adding fields is the mechanical part,
/// but every field widens the diff equality surface and the SGR state
/// machine's emit path, so we don't preemptively carry what we don't use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellStyle {
    /// Foreground colour via crossterm SGR. `None` = terminal default
    /// foreground (emitted as `\x1b[39m` by the serialiser).
    pub fg: Option<Color>,
    /// SGR bold (`\x1b[1m` / `\x1b[22m`).
    pub bold: bool,
    /// SGR reverse video (`\x1b[7m` / `\x1b[27m`). Used for the
    /// highlighted menu row.
    pub reverse: bool,
}

/// One screen cell: glyph + its visual attributes. Cell equality is
/// byte-perfect — two cells are equal iff their serialised bytes
/// would be identical, which is the invariant the diff relies on.
///
/// `width` is the **display width** in terminal columns: 1 for ASCII
/// and other narrow glyphs, 2 for CJK / emoji / other wide glyphs,
/// and 0 for **continuation cells** — placeholder cells that follow a
/// wide glyph to keep the invariant `cell_index == terminal_column`.
/// Without continuation cells, typing "你是谁" (3 wide chars = 6 cols)
/// into a row model that tracked only char count (3 cells) would emit
/// patches at model cols 5/6/7 while the terminal had just advanced
/// to actual col 11 after the first `你`, overwriting each preceding
/// glyph's right half with the next glyph — the "you3-type-shows-only-
/// last-char" bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: CellStyle,
    pub width: u8,
}

impl Default for Cell {
    /// Default blank cell = ASCII space, width 1, default style.
    fn default() -> Self {
        Self {
            ch: ' ',
            style: CellStyle::default(),
            width: 1,
        }
    }
}

impl Cell {
    /// Blank narrow cell — space, width 1. Used for padding and as
    /// the diff's "erase" glyph.
    pub fn blank() -> Self {
        Self::default()
    }

    /// Continuation cell — placeholder for the 2nd (or 3rd, if any)
    /// terminal column occupied by a wide glyph. `width = 0` tells
    /// `serialize_patches` to skip emit for this cell: the wide
    /// glyph emitted in the cell immediately before has already
    /// advanced the terminal cursor past this column.
    pub fn continuation() -> Self {
        Self {
            ch: ' ',
            style: CellStyle::default(),
            width: 0,
        }
    }
}

/// Append each char of `s` as cells, all sharing `style`. Wide chars
/// (CJK, emoji, etc.) expand to one real cell carrying the glyph +
/// `(display_width - 1)` continuation cells so `cell_index ==
/// terminal_column` holds across the row — critical for the cell-diff
/// to produce correct patches.
pub fn push_str_cells(row: &mut Vec<Cell>, s: &str, style: &CellStyle) {
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if w == 0 {
            // Zero-width (combining marks, control chars). Caller has
            // already scrubbed real controls; skip here rather than
            // emit a phantom cell that diff can't align.
            continue;
        }
        row.push(Cell {
            ch,
            style: style.clone(),
            width: w as u8,
        });
        for _ in 1..w {
            row.push(Cell::continuation());
        }
    }
}

/// A single cell's worth of change: "put this cell at absolute position
/// (row, col)". Multiple adjacent patches with the same style serialise
/// into one cursor move + a run of characters, so small clusters stay
/// cheap. Rows/cols are 1-indexed to match ANSI (`\x1b[row;col H`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub row: u16,
    pub col: u16,
    pub cell: Cell,
}

/// Diff two frames expressed as `{absolute_row -> row_cells}`. Returns
/// every cell position where the glyph or style changed. Callers map
/// footer rows to absolute terminal rows (`footer_top + offset`) before
/// calling this so height changes — footer growing from 5 to 9 — are
/// naturally absorbed: a row number that only appears in `next` is all
/// new cells; a row number in both is cell-by-cell diffed; a row only
/// in `prev` is implicitly wiped by the following footer paint that
/// covers its absolute position (or by explicit scroll-region clears).
///
/// Column width mismatches (prev row is shorter than next) get
/// padded: treat missing prev cells as `Cell::blank()`, so the extra
/// tail in `next` emits naturally. When `next` is shorter than `prev`,
/// the trailing `prev` cells become patches to blank (erasing those
/// glyphs). This is important when a menu row shrinks — we need to
/// emit spaces to overwrite leftover characters from the previous
/// paint.
pub fn diff_cells(
    prev: &HashMap<u16, Vec<Cell>>,
    next: &HashMap<u16, Vec<Cell>>,
) -> Vec<Patch> {
    let mut patches = Vec::new();

    // Union of row numbers — every row appearing in either frame needs
    // at least one pass. Sort so output is deterministic (and cursor
    // moves go top-to-bottom, which is cheaper on most terminals'
    // scroll-detection heuristics).
    let mut rows: Vec<u16> = prev.keys().chain(next.keys()).copied().collect();
    rows.sort_unstable();
    rows.dedup();

    for row in rows {
        let empty = Vec::new();
        let p = prev.get(&row).unwrap_or(&empty);
        let n = next.get(&row).unwrap_or(&empty);
        let max_cols = p.len().max(n.len());
        let blank = Cell::blank();
        for col_idx in 0..max_cols {
            let pc = p.get(col_idx).unwrap_or(&blank);
            let nc = n.get(col_idx).unwrap_or(&blank);
            if pc != nc {
                patches.push(Patch {
                    row,
                    col: (col_idx + 1) as u16, // 1-indexed
                    cell: nc.clone(),
                });
            }
        }
    }

    patches
}

/// Serialise patches into ANSI bytes with an SGR state machine: emit
/// cursor-position only when we're jumping, emit SGR only when the
/// outgoing cell's style differs from the last one we set, and run-pack
/// adjacent same-style patches into contiguous character streams.
///
/// Ends with `\x1b[0m` so the caller's subsequent writes (body text,
/// cursor positioning, etc.) start from a clean SGR state — leaving a
/// bold/reverse bit set across paint boundaries was a class of rare
/// but hard-to-reproduce "random colour leak" bugs in the old path.
pub fn serialize_patches(patches: &[Patch]) -> Vec<u8> {
    if patches.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(patches.len() * 8);
    let mut current_style: Option<CellStyle> = None;
    let mut expected_cursor: Option<(u16, u16)> = None;
    let mut emitted_any_sgr = false;

    for patch in patches {
        // Continuation cell: the wide glyph in the previous cell has
        // already advanced the terminal cursor past this column. Emit
        // nothing — writing here would clobber the wide glyph's right
        // half *and* scramble our cursor model.
        if patch.cell.width == 0 {
            continue;
        }

        if expected_cursor != Some((patch.row, patch.col)) {
            let _ = write!(out, "\x1b[{};{}H", patch.row, patch.col);
            expected_cursor = Some((patch.row, patch.col));
        }

        if current_style.as_ref() != Some(&patch.cell.style) {
            let before = out.len();
            emit_sgr_transition(&mut out, current_style.as_ref(), &patch.cell.style);
            if out.len() > before {
                emitted_any_sgr = true;
            }
            current_style = Some(patch.cell.style.clone());
        }

        let mut buf = [0u8; 4];
        let encoded = patch.cell.ch.encode_utf8(&mut buf);
        out.extend_from_slice(encoded.as_bytes());

        // Cursor advances by the glyph's display width. For narrow
        // cells this is +1 (the common case), for wide cells (CJK,
        // emoji) it's +2 — matching what the terminal actually does
        // so the next patch's `expected_cursor` comparison is sound.
        if let Some((r, c)) = expected_cursor {
            expected_cursor = Some((r, c + patch.cell.width as u16));
        }
    }

    // Final `\x1b[0m` only if we ever turned an attribute on — otherwise
    // we'd leak a pointless reset into the stream every time the footer
    // is pure-default-style (all-blank padding, plain rule without
    // colour, etc.). The legacy `row_to_bytes` case exercises this in
    // its tests.
    if emitted_any_sgr {
        out.extend_from_slice(b"\x1b[0m");
    }

    out
}

/// Emit the minimal SGR sequence to move from `from` style to `to` style.
/// Uses reset-and-reapply whenever a "sticky" attribute (bold/reverse)
/// needs clearing; per-attr toggles (`\x1b[22m` for bold off, `\x1b[27m`
/// for reverse off) are respected by modern terminals but reset+reapply
/// is shorter when multiple attributes change at once.
fn emit_sgr_transition(out: &mut Vec<u8>, from: Option<&CellStyle>, to: &CellStyle) {
    let from_default = CellStyle::default();
    let from = from.unwrap_or(&from_default);

    // Determine if any attribute is being turned OFF — if so, cheapest
    // path is reset everything and reapply the ON set. If only
    // additive, use targeted enables.
    let bold_off = from.bold && !to.bold;
    let reverse_off = from.reverse && !to.reverse;
    let fg_change = from.fg != to.fg;

    let needs_reset = bold_off || reverse_off || (from.fg.is_some() && to.fg.is_none());

    if needs_reset {
        out.extend_from_slice(b"\x1b[0m");
        // After reset, nothing is on — apply `to`'s positive attrs.
        if to.bold {
            out.extend_from_slice(b"\x1b[1m");
        }
        if to.reverse {
            out.extend_from_slice(b"\x1b[7m");
        }
        if let Some(c) = to.fg {
            let _ = write!(out, "{}", SetForegroundColor(c));
        }
    } else {
        // Additive path — current attributes stay, just flip on whatever
        // `to` adds.
        if !from.bold && to.bold {
            out.extend_from_slice(b"\x1b[1m");
        }
        if !from.reverse && to.reverse {
            out.extend_from_slice(b"\x1b[7m");
        }
        if fg_change {
            if let Some(c) = to.fg {
                let _ = write!(out, "{}", SetForegroundColor(c));
            } else {
                // Should have been caught by needs_reset, but defensive.
                out.extend_from_slice(b"\x1b[39m");
            }
        }
    }
}

/// Turn `rows: Vec<Vec<Cell>>` (indexed by footer-relative offset) into
/// `{absolute_row -> cells}` for diffing against a prior frame. Rows
/// get placed starting at `base_row` (the footer's top row in absolute
/// screen coordinates, 1-indexed).
pub fn rows_to_frame(rows: &[Vec<Cell>], base_row: u16) -> HashMap<u16, Vec<Cell>> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| (base_row + i as u16, r.clone()))
        .collect()
}

/// Serialise a single row of cells to ANSI bytes — used by legacy
/// (non-DECSTBM) paths that still emit rows whole rather than diffing.
/// Equivalent to the old `build_*_row -> Vec<u8>` output.
pub fn row_to_bytes(cells: &[Cell]) -> Vec<u8> {
    if cells.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(cells.len() * 2);
    let mut current_style: Option<CellStyle> = None;
    let mut emitted_any_sgr = false;
    for cell in cells {
        // Continuation cell: see `serialize_patches` for rationale.
        if cell.width == 0 {
            continue;
        }
        if current_style.as_ref() != Some(&cell.style) {
            let before = out.len();
            emit_sgr_transition(&mut out, current_style.as_ref(), &cell.style);
            if out.len() > before {
                emitted_any_sgr = true;
            }
            current_style = Some(cell.style.clone());
        }
        let mut buf = [0u8; 4];
        let encoded = cell.ch.encode_utf8(&mut buf);
        out.extend_from_slice(encoded.as_bytes());
    }
    if emitted_any_sgr {
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cyan() -> Color {
        Color::Cyan
    }

    fn style_bold_cyan() -> CellStyle {
        CellStyle {
            fg: Some(cyan()),
            bold: true,
            reverse: false,
        }
    }

    #[test]
    fn cell_equality_is_field_wise() {
        let a = Cell {
            ch: 'x',
            style: style_bold_cyan(),
            width: 1,
        };
        let b = Cell {
            ch: 'x',
            style: style_bold_cyan(),
            width: 1,
        };
        assert_eq!(a, b);
        let c = Cell {
            ch: 'y',
            style: style_bold_cyan(),
            width: 1,
        };
        assert_ne!(a, c);
    }

    #[test]
    fn push_str_cells_spreads_one_char_per_cell() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "ab", &CellStyle::default());
        assert_eq!(row.len(), 2);
        assert_eq!(row[0].ch, 'a');
        assert_eq!(row[1].ch, 'b');
    }

    #[test]
    fn diff_emits_only_changed_cells() {
        // Two frames differing in one cell (col 3 of row 5).
        let mut prev = HashMap::new();
        let mut prev_row: Vec<Cell> = "hello".chars().map(|ch| Cell { ch, style: Default::default(), width: 1 }).collect();
        prev.insert(5u16, prev_row.clone());

        let mut next = HashMap::new();
        prev_row[2].ch = 'X'; // change middle char
        next.insert(5u16, prev_row);

        let patches = diff_cells(&prev, &next);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].row, 5);
        assert_eq!(patches[0].col, 3); // 1-indexed
        assert_eq!(patches[0].cell.ch, 'X');
    }

    #[test]
    fn diff_skips_identical_frames() {
        let row: Vec<Cell> = "same"
            .chars()
            .map(|ch| Cell { ch, style: Default::default(), width: 1 })
            .collect();
        let mut prev = HashMap::new();
        prev.insert(1u16, row.clone());
        let mut next = HashMap::new();
        next.insert(1u16, row);
        assert!(diff_cells(&prev, &next).is_empty());
    }

    #[test]
    fn diff_shorter_next_emits_blanks_for_trailing() {
        // prev has 5 cells, next has 2 — the 3 tail cells in prev need
        // blanking patches so leftover glyphs get overwritten.
        let prev_row: Vec<Cell> = "hello"
            .chars()
            .map(|ch| Cell { ch, style: Default::default(), width: 1 })
            .collect();
        let next_row: Vec<Cell> = "he"
            .chars()
            .map(|ch| Cell { ch, style: Default::default(), width: 1 })
            .collect();
        let mut prev = HashMap::new();
        prev.insert(1u16, prev_row);
        let mut next = HashMap::new();
        next.insert(1u16, next_row);
        let patches = diff_cells(&prev, &next);
        assert_eq!(patches.len(), 3);
        for p in &patches {
            assert_eq!(p.cell, Cell::blank());
        }
    }

    #[test]
    fn serialize_empty_patches_emits_nothing() {
        assert!(serialize_patches(&[]).is_empty());
    }

    #[test]
    fn serialize_single_patch_emits_cursor_plus_char() {
        let p = Patch {
            row: 10,
            col: 5,
            cell: Cell {
                ch: 'x',
                style: Default::default(),
                width: 1,
            },
        };
        let bytes = serialize_patches(std::slice::from_ref(&p));
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\x1b[10;5H"));
        assert!(s.contains('x'));
        // Default-style cell → no SGR was turned on, so no trailing
        // \x1b[0m is needed (would be a wasted 4 bytes per emit).
        assert!(!s.contains("\x1b[0m"));
    }

    #[test]
    fn serialize_final_reset_on_styled_patches() {
        // When a patch carries a non-default style, the emit path MUST
        // close with \x1b[0m so subsequent writes start clean.
        let p = Patch {
            row: 1,
            col: 1,
            cell: Cell {
                ch: 'x',
                style: style_bold_cyan(),
                width: 1,
            },
        };
        let bytes = serialize_patches(std::slice::from_ref(&p));
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.ends_with("\x1b[0m"));
    }

    #[test]
    fn serialize_adjacent_cells_skip_cursor_move() {
        // Two patches at (5, 1) and (5, 2) with same default style —
        // second should NOT emit a cursor move (cursor auto-advanced)
        // AND no final reset (default style, no SGR on).
        let p1 = Patch {
            row: 5,
            col: 1,
            cell: Cell { ch: 'a', style: Default::default(), width: 1 },
        };
        let p2 = Patch {
            row: 5,
            col: 2,
            cell: Cell { ch: 'b', style: Default::default(), width: 1 },
        };
        let bytes = serialize_patches(&[p1, p2]);
        let s = String::from_utf8(bytes).unwrap();
        // Exactly one CSI: `\x1b[5;1H`. No SGR, no final reset.
        assert_eq!(s.matches("\x1b[").count(), 1);
    }

    #[test]
    fn serialize_style_change_only_emits_sgr_once() {
        // Two patches at (5,1) and (5,2), second changes to bold —
        // should emit one SGR transition, not two.
        let p1 = Patch {
            row: 5,
            col: 1,
            cell: Cell { ch: 'a', style: Default::default(), width: 1 },
        };
        let p2 = Patch {
            row: 5,
            col: 2,
            cell: Cell {
                ch: 'b',
                style: CellStyle { fg: None, bold: true, reverse: false },
                width: 1,
            },
        };
        let bytes = serialize_patches(&[p1, p2]);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\x1b[1m"), "expected bold SGR, got: {:?}", s);
    }

    #[test]
    fn row_to_bytes_collapses_runs() {
        let row: Vec<Cell> = (0..5)
            .map(|_| Cell { ch: '─', style: Default::default(), width: 1 })
            .collect();
        let bytes = row_to_bytes(&row);
        let s = String::from_utf8(bytes).unwrap();
        // Five UTF-8 ─ (3 bytes each) + nothing else (default style = no SGR).
        assert_eq!(s, "─────");
    }
}
