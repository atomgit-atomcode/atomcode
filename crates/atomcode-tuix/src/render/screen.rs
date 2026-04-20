// crates/atomcode-tuix/src/render/screen.rs
//
// Retained-mode screen buffer — the backbone of the Ink-style
// renderer. Owns two parallel `W × H` cell grids:
//
//   * `cells`      — the frame we are *currently building*. Widget
//                    draws (footer / body / menu) mutate this before
//                    `render_diff` is called.
//   * `prev_cells` — the frame we *last emitted to the terminal*.
//                    Diff basis for the next paint.
//
// `render_diff` computes the patch stream from (prev → current),
// serialises it to ANSI bytes, swaps the frames (current becomes
// prev, prev becomes the fresh scratch we'll next rebuild into) and
// blanks the new scratch so partial draws don't leave stale cells.
//
// Design notes vs. the previous immediate-mode path:
//
//   * **No DECSTBM scroll region**: footer and body share one grid.
//     Scrolling the body is `scroll_up(bottom, n)` — an O(bottom)
//     memcpy inside the grid; terminal-side scrolling happens only
//     via the diff (blank cells appear at the bottom, content that
//     was there now lives higher).
//
//   * **No separate cache invalidation path**: `invalidate()` fills
//     `prev_cells` with blanks so the next `render_diff` emits
//     everything currently in `cells` as if cold-starting. Covers
//     resume-from-external, resize, and any "terminal state is
//     unknown" situation uniformly.
//
//   * **Cursor and visibility** are frame-level state, emitted once
//     per diff at the tail of the patch stream, so they don't
//     bounce around between cell writes.

use std::io::Write as _;

use super::cell::{diff_cell_frames, serialize_patches, Cell};

/// Retained W×H cell grid + current/prev frames.
///
/// Indexing: `cells[row][col]` with `row ∈ 0..height`,
/// `col ∈ 0..width`. ANSI emit converts to 1-indexed at the
/// boundary.
pub struct Screen {
    cells: Vec<Vec<Cell>>,
    prev_cells: Vec<Vec<Cell>>,
    width: u16,
    height: u16,
    /// Where to park the terminal cursor after the frame emits.
    /// `None` means "leave it wherever the last patch left it" —
    /// typically only useful in tests.
    cursor: Option<(u16, u16)>,
    cursor_visible: bool,
}

impl Screen {
    pub fn new(width: u16, height: u16) -> Self {
        let row = vec![Cell::blank(); width as usize];
        let frame = vec![row; height as usize];
        Self {
            cells: frame.clone(),
            prev_cells: frame,
            width,
            height,
            cursor: None,
            cursor_visible: true,
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// Reset every cell of the current frame to a blank with default
    /// style. O(W·H). Typically called by `render_diff` after a swap
    /// so the next draw cycle starts from a clean scratch.
    pub fn clear(&mut self) {
        let blank = Cell::blank();
        for row in &mut self.cells {
            for c in row {
                *c = blank.clone();
            }
        }
    }

    /// Write `cells` starting at `(row, col)` in the current frame.
    /// Out-of-bounds rows are silently skipped (so callers don't
    /// need to clamp every time); cols beyond `width` are truncated
    /// to the right edge.
    ///
    /// Cells with `width == 2` (wide CJK / emoji) should have a
    /// following `Cell::continuation()` from the caller — this method
    /// itself doesn't auto-insert them. `push_str_cells` on the
    /// caller side handles that invariant.
    pub fn draw_row(&mut self, row: usize, col: usize, cells: &[Cell]) {
        if row >= self.cells.len() {
            return;
        }
        let target = &mut self.cells[row];
        for (i, cell) in cells.iter().enumerate() {
            let dst_col = col + i;
            if dst_col >= target.len() {
                break;
            }
            target[dst_col] = cell.clone();
        }
    }

    /// Park the terminal cursor at `(row, col)` (1-indexed ANSI
    /// coords) at the end of the next `render_diff`. Typically
    /// pointed at the input prompt's insertion cell.
    pub fn set_cursor(&mut self, row: u16, col: u16) {
        self.cursor = Some((row, col));
    }

    /// Scroll the top `bottom` rows up by `n`. Rows `[0..n)` are
    /// dropped; rows `[n..bottom)` slide to `[0..bottom-n)`; rows
    /// `[bottom-n..bottom)` become blank, ready for new content.
    /// Rows `[bottom..height)` (typically the fixed footer) are
    /// untouched.
    ///
    /// Used for body "append a line" semantics in retained mode:
    /// scroll the whole body region up by one, then draw the new
    /// line at `bottom - 1`.
    pub fn scroll_up(&mut self, bottom: usize, n: usize) {
        if n == 0 || bottom == 0 {
            return;
        }
        let n = n.min(bottom);
        let blank_row = vec![Cell::blank(); self.width as usize];
        // `rotate_left` on the `[0..bottom)` slice slides the first
        // `n` rows to the end of the slice — logically "scroll up".
        // `Vec<Cell>` isn't `Copy`, so `copy_within` won't work;
        // `rotate_left` moves (not copies) so it's valid for owned
        // row vectors.
        self.cells[0..bottom].rotate_left(n);
        // The rows we just rotated to the end of the window hold
        // stale content (what was at the top). Blank them for new
        // content to land into.
        for row_idx in (bottom - n)..bottom {
            self.cells[row_idx] = blank_row.clone();
        }
    }

    /// Produce the ANSI patch stream for (prev → current). Swaps
    /// frames at the end so the `cells` we just rendered becomes
    /// the next diff's `prev_cells`. Scratches `cells` to blank so
    /// the next draw cycle starts clean — callers must re-draw
    /// every widget every frame (retained-mode invariant).
    pub fn render_diff(&mut self) -> Vec<u8> {
        let patches = diff_cell_frames(&self.prev_cells, &self.cells);
        let mut out = serialize_patches(&patches);
        if let Some((r, c)) = self.cursor {
            let _ = write!(&mut out, "\x1b[{};{}H", r, c);
        }
        if self.cursor_visible {
            out.extend_from_slice(b"\x1b[?25h");
        } else {
            out.extend_from_slice(b"\x1b[?25l");
        }
        std::mem::swap(&mut self.prev_cells, &mut self.cells);
        // Clear the new scratch. Without this, stale cells from
        // N frames ago would be diffed against next frame and
        // generate patches that erase content that actually
        // belongs on screen.
        self.clear();
        out
    }

    /// Force the next `render_diff` to emit every non-blank cell as
    /// if prev were all-blank. Called after `resume_from_external`,
    /// `resize`, or any other event that leaves terminal state
    /// unknown. Safe to call even when prev is already blank
    /// (just produces no additional emit).
    pub fn invalidate(&mut self) {
        let blank_row = vec![Cell::blank(); self.width as usize];
        for row in &mut self.prev_cells {
            *row = blank_row.clone();
        }
    }

    /// Rebuild for new dimensions. Current and prev frames are
    /// discarded — the caller must re-draw every widget before
    /// the next `render_diff`.
    pub fn resize(&mut self, width: u16, height: u16) {
        *self = Self::new(width, height);
    }

    /// Peek at the last-emitted frame. Used by tests and the
    /// diagnostic trace path (`tuix_trace!("FOOT", ...)`) to
    /// inspect "what is actually on screen right now" without
    /// reconstructing state from the ANSI byte stream. Not meant
    /// for normal rendering — that goes through `render_diff`.
    pub fn prev_cells_for_test(&self) -> &[Vec<Cell>] {
        &self.prev_cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::cell::{push_str_cells, CellStyle};

    #[test]
    fn new_screen_empty_frame_produces_no_content_patches() {
        // Two all-blank frames → diff emits zero cell patches. Only
        // trailing cursor-visibility control survives (the SGR reset
        // also does NOT emit because serialize_patches skips it when
        // no SGR was ever turned on).
        let mut s = Screen::new(10, 3);
        let bytes = s.render_diff();
        let out = String::from_utf8(bytes).unwrap();
        // Expect exactly the cursor-show sequence, nothing else.
        assert_eq!(out, "\x1b[?25h", "unexpected bytes: {:?}", out);
    }

    #[test]
    fn draw_row_emits_content_at_1_indexed_coords() {
        let mut s = Screen::new(20, 5);
        let mut cells = Vec::new();
        push_str_cells(&mut cells, "hello", &CellStyle::default());
        s.draw_row(2, 3, &cells);
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(out.contains("hello"), "missing content: {:?}", out);
        // Row 2 (0-indexed) → ANSI row 3; col 3 → ANSI col 4.
        assert!(out.contains("\x1b[3;4H"), "wrong cursor target: {:?}", out);
    }

    #[test]
    fn second_frame_with_same_content_emits_no_cells() {
        let mut s = Screen::new(20, 5);
        let mut cells = Vec::new();
        push_str_cells(&mut cells, "x", &CellStyle::default());
        s.draw_row(0, 0, &cells);
        let _ = s.render_diff(); // first frame emits 'x'
        // Redraw identical content — the render_diff above cleared
        // the scratch to blank, so we need to re-push.
        s.draw_row(0, 0, &cells);
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(
            !out.contains('x'),
            "identical re-draw should be a no-op diff: {:?}",
            out
        );
    }

    #[test]
    fn scroll_up_shifts_rows_drops_top() {
        let mut s = Screen::new(10, 5);
        let mut a = Vec::new();
        push_str_cells(&mut a, "AAA", &CellStyle::default());
        let mut b = Vec::new();
        push_str_cells(&mut b, "BBB", &CellStyle::default());
        // Populate rows 0, 1.
        s.draw_row(0, 0, &a);
        s.draw_row(1, 0, &b);
        let _ = s.render_diff(); // swaps into prev, clears scratch
        // Re-draw the same content then scroll.
        s.draw_row(0, 0, &a);
        s.draw_row(1, 0, &b);
        s.scroll_up(2, 1);
        // After scroll_up(bottom=2, n=1):
        //   cells[0] = what was cells[1] = "BBB"
        //   cells[1] = blank
        // Diff against prev (row0="AAA", row1="BBB") →
        //   row 0: prev "AAA" vs now "BBB" → patches
        //   row 1: prev "BBB" vs now blank → blank patches
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(out.contains("BBB"), "row 0 should now show BBB");
    }

    #[test]
    fn invalidate_forces_cold_start_on_next_diff() {
        let mut s = Screen::new(10, 3);
        let mut cells = Vec::new();
        push_str_cells(&mut cells, "hi", &CellStyle::default());
        s.draw_row(0, 0, &cells);
        let _ = s.render_diff();
        // Same content, but invalidate → next diff emits full
        // cold-start patches for every non-blank cell.
        s.draw_row(0, 0, &cells);
        s.invalidate();
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(
            out.contains("hi"),
            "invalidate must force re-emit: {:?}",
            out
        );
    }

    #[test]
    fn resize_blanks_both_frames() {
        let mut s = Screen::new(10, 3);
        let mut cells = Vec::new();
        push_str_cells(&mut cells, "stuff", &CellStyle::default());
        s.draw_row(0, 0, &cells);
        let _ = s.render_diff();
        s.resize(20, 5);
        assert_eq!(s.width(), 20);
        assert_eq!(s.height(), 5);
        // After resize, drawing no content → empty diff.
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(
            !out.contains("stuff"),
            "old content must be gone after resize: {:?}",
            out
        );
    }

    #[test]
    fn set_cursor_emits_final_position() {
        let mut s = Screen::new(10, 3);
        s.set_cursor(2, 5);
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(
            out.contains("\x1b[2;5H"),
            "cursor park missing: {:?}",
            out
        );
    }
}
