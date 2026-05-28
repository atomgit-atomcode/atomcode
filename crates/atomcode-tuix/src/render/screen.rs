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
//   * **Cursor and visibility** are frame-level state. Visibility is
//     hidden at the head of the diff and restored at the tail; cursor
//     position parks at the tail. Never interleaved with cell writes,
//     so the caret can't be seen jumping across the screen between
//     patches. Frames that emit no work skip the wrap entirely.

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
    /// Set when the physical terminal state is known to be out of sync
    /// with `prev_cells` — typically because a caller invoked
    /// `invalidate()` after a direct-stdout write or session-state
    /// change. The next `render_diff` prepends a per-row CUP+EL so the
    /// terminal starts from a known-blank canvas before the diff
    /// patches put content back. Without this, the cell-diff alone
    /// can't fix the staleness: if the new frame's `cells` are blank
    /// at a column whose `prev_cells` is ALSO blank (post-invalidate),
    /// no patch is emitted and the stale physical glyph survives —
    /// the "right-edge ghost row tail" symptom.
    physical_dirty: bool,
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
            physical_dirty: false,
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

    /// Toggle DECTCEM cursor visibility for the next `render_diff`.
    /// Used to hide the cursor while a live body spinner is animating
    /// (otherwise it sits at the end of "Pondering… · 5s" and blinks).
    /// `render_diff` re-emits this every frame, so flipping the flag
    /// once is enough — every subsequent paint reasserts it.
    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    /// Read the currently-set cursor position (if any). Used by callers
    /// that emitted out-of-band writes after the last render_diff and
    /// want to re-anchor the terminal cursor without doing another full
    /// diff cycle.
    pub fn peek_cursor(&self) -> Option<(u16, u16)> {
        self.cursor
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
        let mut body = Vec::new();
        let cold_start = self.physical_dirty;
        if self.physical_dirty {
            // Cold-start the physical terminal: per-row CUP+EL across
            // every screen row, then `\x1b[H` to home. The subsequent
            // diff patches put cells's content back. This costs ~8
            // bytes per row plus the home — only paid when callers
            // explicitly signal that physical state is unknown via
            // `invalidate()`. Without this, body rows whose cells go
            // from "long content" (last frame) to "short content"
            // (this frame, with `prev_cells` blanked by invalidate)
            // leave the long row's tail on the physical terminal
            // forever — the cell-diff sees `cells=blank == prev=blank`
            // for the trailing columns and emits no patch, so the
            // stale glyphs survive.
            let h = self.cells.len();
            body.reserve(h * 8 + 4);
            for row in 1..=h {
                let _ = write!(&mut body, "\x1b[{};1H\x1b[K", row);
            }
            body.extend_from_slice(b"\x1b[H");
            self.physical_dirty = false;
        }
        let patches = diff_cell_frames(&self.prev_cells, &self.cells);
        let patch_bytes = serialize_patches(&patches);
        body.extend_from_slice(&patch_bytes);
        // Anti-flicker wrap around any frame that moves the caret.
        // `serialize_patches` walks the cursor across every changed
        // cell via CUP+glyph; the next frame parks it back at the
        // input prompt. With DECTCEM on throughout, on streaming
        // bodies the user perceives the caret zooming across the
        // screen at 50fps and snapping back — looks like the input
        // box is "blinking". `?25l` at frame start keeps the caret
        // invisible during the patch walk; DECSET 2026 (synchronized
        // output) lets capable hosts — Windows Terminal, iTerm2,
        // kitty, wezterm, alacritty — defer paint until the whole
        // patch lands, hiding intermediate state entirely. Older
        // terminals ignore unknown DEC private modes per spec.
        let has_visible_work = cold_start || !patch_bytes.is_empty();
        let mut out = Vec::with_capacity(body.len() + 32);
        if has_visible_work {
            out.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        }
        out.extend_from_slice(&body);
        if let Some((r, c)) = self.cursor {
            let _ = write!(&mut out, "\x1b[{};{}H", r, c);
        }
        if self.cursor_visible {
            out.extend_from_slice(b"\x1b[?25h");
        } else {
            out.extend_from_slice(b"\x1b[?25l");
        }
        if has_visible_work {
            out.extend_from_slice(b"\x1b[?2026l");
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
        // Sentinel (not Cell::blank) so the next diff sees EVERY cell as
        // changed — including positions where both the stale frame and
        // the upcoming next frame happen to hold default-style spaces.
        // The blank-match-blank suppression was the leak source for the
        // win10 + pwsh7 + zh_CN char-doubling bug: direct-write left
        // stale glyphs at columns the diff then declined to repaint.
        // See `Cell::sentinel` for the full write-up.
        let sentinel_row = vec![Cell::sentinel(); self.width as usize];
        for row in &mut self.prev_cells {
            *row = sentinel_row.clone();
        }
        // Mark physical state unknown so the next render_diff begins
        // with a cold-start per-row CUP+EL — see `Screen::physical_dirty`
        // for the failure mode this guards against (right-edge ghost
        // tail after invalidate + shrink).
        self.physical_dirty = true;
    }

    /// Blank `prev_cells` for rows `[start_row, height)`. Used by callers
    /// that wrote `\x1b[0J` (erase-to-end-of-display) directly to stdout
    /// from `start_row` down: the physical terminal is now blank in that
    /// region, but `prev_cells` still holds the cells of whatever was
    /// there last frame. Without resyncing, the next `render_diff` may
    /// suppress a patch for a row whose new cells COINCIDENTALLY match
    /// the stale `prev_cells` (e.g. a top_rule full of `─` lining up
    /// with a stale bot_rule full of the same `─`) — and the row stays
    /// physically blank because the diff thinks no change is needed.
    ///
    /// Mirrors `invalidate()` (which blanks everything) and
    /// `shift_prev_up()` (which scrolls then blanks the bottom n rows)
    /// — same shape, different region. The companion direct-stdout
    /// write is the caller's responsibility; this method only updates
    /// the diff cache to match what the caller already told the
    /// terminal to do.
    pub fn invalidate_rows_from(&mut self, start_row: usize) {
        // Sentinel — see `invalidate` and `Cell::sentinel` for why this
        // is NOT `Cell::blank`. This is the main hot path for the bug:
        // every push_body_row → emit_body_line_inner direct-write hits
        // this. With Cell::blank prev, the follow-up cell-diff treated
        // a row of `   X   Y   ` as "only patch X and Y, the spaces
        // already match" — letting stale wide-char right-halves from a
        // 1-col-off direct-write linger. Sentinel forces every column
        // through the diff, including the spaces.
        let sentinel_row = vec![Cell::sentinel(); self.width as usize];
        let h = self.prev_cells.len();
        for r in start_row.min(h)..h {
            self.prev_cells[r] = sentinel_row.clone();
        }
    }

    /// Shift the recorded `prev_cells` up by `n` rows, blanking the
    /// freed rows at the bottom. Used when the caller has triggered
    /// a TERMINAL-side scroll (e.g. emitted `CUP(h,1) + LF` to push
    /// the visible top into native scrollback): the physical terminal
    /// state has shifted but our prev-frame cache hasn't. Re-syncing
    /// here lets the next `render_diff` compute correct deltas
    /// against reality instead of emitting a full-frame repaint.
    ///
    /// Mirrors `scroll_up`'s rotate-then-blank shape but operates on
    /// `prev_cells` (the diff basis), not `cells` (the next-frame
    /// scratch). `cells` is left untouched because the standard paint
    /// cycle clears it at the end of every `render_diff` anyway —
    /// the next `paint_frame` re-populates it from scratch.
    pub fn shift_prev_up(&mut self, n: usize) {
        let h = self.prev_cells.len();
        if n == 0 || h == 0 {
            return;
        }
        let n = n.min(h);
        // Sentinel for the freed-by-scroll rows — same reasoning as the
        // other invalidate paths. The caller just told the terminal "LF
        // at the bottom row" which promoted the top into native
        // scrollback and left the bottom slot blank; we sentinel-fill
        // that slot so the next render_diff repaints every cell of it,
        // not just the non-space ones.
        let sentinel_row = vec![Cell::sentinel(); self.width as usize];
        self.prev_cells.rotate_left(n);
        for row_idx in (h - n)..h {
            self.prev_cells[row_idx] = sentinel_row.clone();
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

    /// `invalidate()` is the documented way for callers to signal
    /// "physical terminal state is unknown — repaint cold". The diff
    /// alone cannot restore correctness in that case: if the new frame's
    /// `cells` are blank at a column whose `prev_cells` was ALSO blanked
    /// by the invalidate, no patch is emitted and the stale physical
    /// glyph survives. The next render_diff after invalidate MUST
    /// therefore prepend a per-row CUP+EL so the terminal cold-starts.
    #[test]
    fn invalidate_prepends_per_row_clear_on_next_diff() {
        let mut s = Screen::new(10, 3);
        // Warm the prev_cells with content so the test mirrors the
        // "previous frame had content, now we invalidate" flow.
        let mut cells = Vec::new();
        push_str_cells(&mut cells, "hi", &CellStyle::default());
        s.draw_row(0, 0, &cells);
        let _ = s.render_diff();
        // Caller invalidates because physical state is now unknown.
        s.invalidate();
        // Next paint cycle: redraw the same content.
        s.draw_row(0, 0, &cells);
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        // Per-row CUP+EL must appear for every row before the content
        // patches — that's the cold-start cue for the physical
        // terminal.
        for row in 1..=3 {
            let needle = format!("\x1b[{};1H\x1b[K", row);
            assert!(
                out.contains(&needle),
                "invalidate should prepend CUP+EL for row {} on next render_diff; got: {:?}",
                row,
                out
            );
        }
        // Content must still emit.
        assert!(
            out.contains("hi"),
            "invalidated content should still re-emit: {:?}",
            out
        );
    }

    /// `invalidate()` is "one-shot": the cold-start cue fires on the
    /// NEXT render_diff and clears the flag, so subsequent frames go
    /// back to the cheap incremental diff path.
    #[test]
    fn invalidate_cold_start_does_not_repeat_across_frames() {
        let mut s = Screen::new(10, 3);
        s.invalidate();
        let _ = s.render_diff(); // consumes the cold-start cue
        // Second frame: no invalidate, no cold start.
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(
            !out.contains("\x1b[1;1H\x1b[K"),
            "no cold-start CUP+EL on second frame after one-shot invalidate: {:?}",
            out
        );
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
    fn shift_prev_up_drops_top_blanks_bottom() {
        // Populate two distinct rows then commit (swap into prev).
        let mut s = Screen::new(10, 4);
        let mut a = Vec::new();
        push_str_cells(&mut a, "AAA", &CellStyle::default());
        let mut b = Vec::new();
        push_str_cells(&mut b, "BBB", &CellStyle::default());
        s.draw_row(0, 0, &a);
        s.draw_row(1, 0, &b);
        let _ = s.render_diff();
        // prev_cells now: row0="AAA", row1="BBB", row2=blank, row3=blank.
        s.shift_prev_up(1);
        // Expect: row0="BBB", row1=blank, row2=blank, row3=blank.
        let prev = s.prev_cells_for_test();
        let row0_text: String = prev[0]
            .iter()
            .filter(|c| c.width > 0)
            .map(|c| c.ch)
            .collect::<String>();
        let row1_text: String = prev[1]
            .iter()
            .filter(|c| c.width > 0)
            .map(|c| c.ch)
            .collect::<String>();
        assert!(
            row0_text.trim_end().starts_with("BBB"),
            "prev row 0 should now be the old row 1 (BBB), got {:?}",
            row0_text
        );
        assert_eq!(
            row1_text.trim_end(),
            "",
            "prev row 1 should be blank after shift, got {:?}",
            row1_text
        );
    }

    #[test]
    fn shift_prev_up_zero_is_noop() {
        let mut s = Screen::new(10, 3);
        let mut a = Vec::new();
        push_str_cells(&mut a, "KEEP", &CellStyle::default());
        s.draw_row(0, 0, &a);
        let _ = s.render_diff();
        s.shift_prev_up(0);
        let row0_text: String = s.prev_cells_for_test()[0]
            .iter()
            .filter(|c| c.width > 0)
            .map(|c| c.ch)
            .collect::<String>();
        assert!(
            row0_text.trim_end().starts_with("KEEP"),
            "shift_prev_up(0) must be a no-op, got {:?}",
            row0_text
        );
    }

    #[test]
    fn set_cursor_emits_final_position() {
        let mut s = Screen::new(10, 3);
        s.set_cursor(2, 5);
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(out.contains("\x1b[2;5H"), "cursor park missing: {:?}", out);
    }

    /// Frames with cell work get wrapped in DECSET 2026 + DECTCEM
    /// hide/show so the caret doesn't visibly bounce across the screen
    /// during the patch walk. Empty frames skip the wrap to avoid
    /// wasting bytes on terminals that don't need it.
    #[test]
    fn nonempty_frame_wraps_in_sync_output_and_hides_cursor() {
        let mut s = Screen::new(10, 3);
        let mut cells = Vec::new();
        push_str_cells(&mut cells, "x", &CellStyle::default());
        s.draw_row(0, 0, &cells);
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        // BSU/ESU wrap exists and order is correct.
        let bsu = out.find("\x1b[?2026h").expect("BSU present");
        let esu = out.find("\x1b[?2026l").expect("ESU present");
        assert!(bsu < esu, "BSU must precede ESU: {:?}", out);
        // Hide-cursor sits immediately after BSU, before any cell write.
        assert!(
            out.starts_with("\x1b[?2026h\x1b[?25l"),
            "frame must open with BSU+hide: {:?}",
            out
        );
        // Visibility restored before ESU.
        assert!(
            out.ends_with("\x1b[?25h\x1b[?2026l"),
            "frame must close with show+ESU: {:?}",
            out
        );
        // Cell content lands inside the wrap.
        let x_pos = out.find('x').expect("x rendered");
        assert!(bsu < x_pos && x_pos < esu, "cell write must sit inside wrap: {:?}", out);
    }

    #[test]
    fn empty_frame_skips_sync_output_wrap() {
        let mut s = Screen::new(10, 3);
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(!out.contains("\x1b[?2026h"), "no BSU on empty frame: {:?}", out);
        assert!(!out.contains("\x1b[?2026l"), "no ESU on empty frame: {:?}", out);
        assert!(!out.contains("\x1b[?25l"), "no hide on empty frame: {:?}", out);
    }

    /// Invalidate triggers a cold-start CUP+EL walk across every row —
    /// the cursor moves a lot. That work must also be wrapped in the
    /// sync/hide envelope.
    #[test]
    fn invalidate_cold_start_is_wrapped() {
        let mut s = Screen::new(10, 3);
        s.invalidate();
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(out.starts_with("\x1b[?2026h\x1b[?25l"), "cold-start frame must open with BSU+hide: {:?}", out);
        assert!(out.ends_with("\x1b[?25h\x1b[?2026l"), "cold-start frame must close with show+ESU: {:?}", out);
    }
}
