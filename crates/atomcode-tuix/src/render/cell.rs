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

use crossterm::style::Color;
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
    /// Background colour via crossterm SGR. `None` = terminal default
    /// background (emitted as `\x1b[49m` by the serialiser).
    pub bg: Option<Color>,
    /// SGR bold (`\x1b[1m` / `\x1b[22m`).
    pub bold: bool,
    /// SGR reverse video (`\x1b[7m` / `\x1b[27m`). Used for the
    /// highlighted menu row.
    pub reverse: bool,
    /// SGR faint / decreased intensity (`\x1b[2m`). Renders the current
    /// fg at ~50% intensity — terminal-theme-aware muting that adapts
    /// to both light and dark schemes (unlike a fixed DarkGrey which
    /// vanishes on some palettes). Toggled off via SGR 22, which is the
    /// shared "normal intensity" reset for both bold and faint, so the
    /// transition path goes through full reset when faint→off.
    pub faint: bool,
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

    /// Sentinel value used only inside `prev_cells` after an invalidate.
    /// Picked so it can never `==` any cell produced by `push_str_cells` /
    /// `push_str_cells_sgr` (which never push U+FFFF — the Unicode
    /// non-character reserved by the standard precisely for in-process
    /// use like this). Forcing the diff to see "every cell differs" after
    /// an invalidate means `serialize_patches` re-emits every position in
    /// the row, including the space cells that previously matched
    /// `Cell::blank()` and went unpatched. That last branch was the
    /// orphan-cell leak source: `emit_body_line_inner` did a direct write
    /// using one width model, the follow-up cell-diff used a slightly
    /// different one (East Asian Ambiguous wobble on conhost), and the
    /// space cells the diff skipped left the direct write's stale content
    /// visible — looked like char-doubling on win10+pwsh7 + zh_CN.
    ///
    /// Cost: every invalidated row pays ~1 extra patch per blank trailing
    /// cell. `serialize_patches`' run-packing folds those into a single
    /// CUP+space-run, so the wire cost is bounded by the row width, not
    /// the cell count. Worth it — width-mismatch bugs degrade to "wrong
    /// glyph in one cell" instead of "every space gets the previous
    /// char", which is much easier to live with.
    pub fn sentinel() -> Self {
        Self {
            ch: '\u{FFFF}',
            style: CellStyle::default(),
            width: 1,
        }
    }
}

/// Fixed soft-tab width — `\t` expands to this many spaces when a
/// caller pushes a string that slipped past higher-level tab-aware
/// paths. Matches claude-code / CC-style tooling conventions.
///
/// `pub(crate)` so width-measurement code (`width::wrap_with_cursor`,
/// the input-box cursor mapping) models a `\t` as exactly this many
/// columns, matching how [`push_str_cells`] actually draws it. A
/// mismatch here desyncs the input caret from the text on tab-indented
/// (pasted) lines.
pub(crate) const SOFT_TAB_WIDTH: usize = 4;

/// Append each char of `s` as cells, all sharing `style`. Wide chars
/// (CJK, emoji, etc.) expand to one real cell carrying the glyph +
/// `(display_width - 1)` continuation cells so `cell_index ==
/// terminal_column` holds across the row — critical for the cell-diff
/// to produce correct patches.
///
/// Control chars that would mis-align the cell model vs the terminal
/// are normalised here:
///   - `\n` / `\r`: dropped. Multi-line content must be split by the
///     caller (`push_body_text` does this); writing a bare LF under
///     raw-mode drops a row without CR, and a bare CR returns to col
///     0 mid-row — both produce the "staircase" bug.
///   - `\t`: expanded to SOFT_TAB_WIDTH spaces so cell col == terminal
///     col. Without this, the terminal jumps to its hardware tab stop
///     (col 9/17/25/…) while our cell model advances 1 col per `\t`
///     cell, and subsequent diffs patch the wrong columns.
pub fn push_str_cells(row: &mut Vec<Cell>, s: &str, style: &CellStyle) {
    for ch in s.chars() {
        if ch == '\n' || ch == '\r' {
            continue;
        }
        if ch == '\t' {
            for _ in 0..SOFT_TAB_WIDTH {
                row.push(Cell {
                    ch: ' ',
                    style: style.clone(),
                    width: 1,
                });
            }
            continue;
        }
        let w = crate::width::cell_char_width(ch).unwrap_or(1);
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

/// Like [`push_str_cells`] but parses embedded SGR escape sequences
/// (`\x1b[...m`) inline, mutating a working `CellStyle` so subsequent
/// cells pick up the colour / bold / faint / reverse attributes the
/// terminal would otherwise paint via raw ANSI. Returns the style
/// state at end-of-input so a caller wrapping a single physical line
/// into multiple chunks can carry attributes across chunk boundaries
/// (e.g. `\x1b[31m` on one chunk and `\x1b[39m` on the next).
///
/// Why this exists: the retained renderer paints from a cell grid
/// rather than streaming raw bytes to stdout, so SGR sequences that
/// survive [`crate::sanitize::scrub_controls_keep_sgr`] would
/// otherwise land as literal `^[[31m` characters in cells. This
/// function is the cell-pipeline equivalent of alt-screen's
/// `truncate_to_width_sgr_aware` — it understands SGR enough to
/// translate it into `CellStyle` mutations on the way in.
///
/// Non-SGR CSI sequences (cursor moves, DSR, etc.) are silently
/// dropped — they should have been scrubbed upstream; this is
/// belt-and-suspenders.
pub fn push_str_cells_sgr(row: &mut Vec<Cell>, s: &str, mut working_style: CellStyle) -> CellStyle {
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                let mut params = String::new();
                let mut final_byte: Option<char> = None;
                while let Some(&p) = chars.peek() {
                    chars.next();
                    if ('\x40'..='\x7E').contains(&p) {
                        final_byte = Some(p);
                        break;
                    }
                    params.push(p);
                }
                if final_byte == Some('m') {
                    apply_sgr_params(&params, &mut working_style);
                }
            }
            continue;
        }
        if ch == '\n' || ch == '\r' {
            continue;
        }
        if ch == '\t' {
            for _ in 0..SOFT_TAB_WIDTH {
                row.push(Cell {
                    ch: ' ',
                    style: working_style.clone(),
                    width: 1,
                });
            }
            continue;
        }
        let w = crate::width::cell_char_width(ch).unwrap_or(1);
        if w == 0 {
            continue;
        }
        row.push(Cell {
            ch,
            style: working_style.clone(),
            width: w as u8,
        });
        for _ in 1..w {
            row.push(Cell::continuation());
        }
    }
    working_style
}

/// Parse a `;`-separated SGR parameter list and fold each recognised
/// code into `style`. The crossterm colour variants chosen here mirror
/// the SGR-to-name mapping that `serialize_row` uses to emit cells
/// back to the terminal — so a row built from `\x1b[31m…` round-trips
/// to `\x1b[31m…` on output, and the terminal's theme palette gets to
/// pick the actual shade rather than us hard-coding RGB.
///
/// Extended colour codes are decoded into crossterm colour variants:
/// 256-colour `38;5;N` / `48;5;N` → `AnsiValue(N)`, and truecolor
/// `38;2;R;G;B` / `48;2;R;G;B` → `Rgb`. These reach the retained path
/// from the modal menu overlay (the `/usage` heatmap and per-model
/// chart paint cells with `\x1b[38;5;Nm█`), so dropping them rendered
/// those cells at the terminal default (white).
///
/// Italic / underline and other codes outside `CellStyle`'s cosmetic
/// surface are still silently skipped — adding a field to `CellStyle`
/// is the trigger for extending this match.
fn apply_sgr_params(params: &str, style: &mut CellStyle) {
    // `\x1b[m` (empty params) means "reset" — same as `\x1b[0m`.
    if params.is_empty() {
        *style = CellStyle::default();
        return;
    }
    let parts: Vec<&str> = params.split(';').collect();
    let mut i = 0;
    while i < parts.len() {
        let code = parts[i];
        // `\x1b[;31m` (leading empty) also resets before applying.
        if code.is_empty() {
            *style = CellStyle::default();
            i += 1;
            continue;
        }
        let Ok(n) = code.parse::<u16>() else {
            i += 1;
            continue;
        };
        match n {
            0 => *style = CellStyle::default(),
            1 => style.bold = true,
            2 => style.faint = true,
            7 => style.reverse = true,
            22 => {
                // SGR 22 = normal intensity, clears BOTH bold and faint.
                style.bold = false;
                style.faint = false;
            }
            27 => style.reverse = false,
            30 => style.fg = Some(Color::Black),
            31 => style.fg = Some(Color::DarkRed),
            32 => style.fg = Some(Color::DarkGreen),
            33 => style.fg = Some(Color::DarkYellow),
            34 => style.fg = Some(Color::DarkBlue),
            35 => style.fg = Some(Color::DarkMagenta),
            36 => style.fg = Some(Color::DarkCyan),
            37 => style.fg = Some(Color::Grey),
            38 => {
                if let Some((c, consumed)) = parse_extended_colour(&parts[i + 1..]) {
                    style.fg = Some(c);
                    i += consumed;
                }
            }
            39 => style.fg = None,
            48 => {
                if let Some((c, consumed)) = parse_extended_colour(&parts[i + 1..]) {
                    style.bg = Some(c);
                    i += consumed;
                }
            }
            49 => style.bg = None,
            90 => style.fg = Some(Color::DarkGrey),
            91 => style.fg = Some(Color::Red),
            92 => style.fg = Some(Color::Green),
            93 => style.fg = Some(Color::Yellow),
            94 => style.fg = Some(Color::Blue),
            95 => style.fg = Some(Color::Magenta),
            96 => style.fg = Some(Color::Cyan),
            97 => style.fg = Some(Color::White),
            _ => {}
        }
        i += 1;
    }
}

/// Decode the sub-parameters that follow an SGR `38` / `48` introducer.
/// `rest` is the slice of params after the introducer. Returns the
/// resolved colour and how many of those sub-parameters were consumed
/// (so the caller can advance past them): `5;N` → 2, `2;R;G;B` → 4.
/// Malformed sequences consume nothing and yield `None`.
fn parse_extended_colour(rest: &[&str]) -> Option<(Color, usize)> {
    match rest.first().copied() {
        Some("5") => {
            let n = rest.get(1)?.parse::<u8>().ok()?;
            Some((Color::AnsiValue(n), 2))
        }
        Some("2") => {
            let r = rest.get(1)?.parse::<u8>().ok()?;
            let g = rest.get(2)?.parse::<u8>().ok()?;
            let b = rest.get(3)?.parse::<u8>().ok()?;
            Some((Color::Rgb { r, g, b }, 4))
        }
        _ => None,
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

/// Slice-based cell-diff for the retained-mode `Screen` buffer.
/// Both frames are `[Vec<Cell>]` indexed by **screen row** (0..H-1),
/// with each inner `Vec<Cell>` indexed by **screen col** (0..W-1).
/// Unlike `diff_cells`, this variant doesn't allocate a hashmap per
/// frame — Screen always knows all its rows upfront, so a contiguous
/// slice is both faster and maps 1:1 onto the 2D `cells[row][col]`
/// access pattern.
///
/// Emits patches with **1-indexed** (row, col) matching ANSI cursor
/// addressing. When one frame is shorter than the other (shouldn't
/// happen in practice if both come from the same `Screen`, but keep
/// the robustness for safety), missing rows / columns are treated as
/// blank so the "other" frame's content generates explicit patches.
pub fn diff_cell_frames(prev: &[Vec<Cell>], next: &[Vec<Cell>]) -> Vec<Patch> {
    let mut patches = Vec::new();
    let max_rows = prev.len().max(next.len());
    let blank = Cell::blank();
    for r in 0..max_rows {
        let p = prev.get(r).map(Vec::as_slice).unwrap_or(&[]);
        let n = next.get(r).map(Vec::as_slice).unwrap_or(&[]);
        let max_cols = p.len().max(n.len());
        for c in 0..max_cols {
            let pc = p.get(c).unwrap_or(&blank);
            let nc = n.get(c).unwrap_or(&blank);
            if pc != nc {
                patches.push(Patch {
                    row: (r + 1) as u16,
                    col: (c + 1) as u16,
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

        // Cursor advance: only safe to model-predict for ASCII (U+0000..U+007F).
        //
        // Non-ASCII chars are where the renderer's `unicode-width` prediction
        // can disagree with the terminal's actual rendering width. The two
        // failure classes we hit in the wild:
        //
        //   * East Asian Ambiguous chars (×, ✓, →, •, 1-byte less than 0x80
        //     but Unicode-wise above 0x80 — yes that's a contradiction by
        //     codepoint, see below): default `unicode-width` predicts 1 col,
        //     but legacy conhost + xterm.js in CJK locale render at 2 cols.
        //   * Wide emoji (💡, etc.): predicted 2 cols, but some Windows
        //     fonts render at 1 col.
        //
        // When prediction is off by N, the next patch's `expected_cursor`
        // comparison thinks no CUP is needed but the terminal cursor is
        // actually N cols away from where the model says. Run-packing then
        // streams subsequent cells straight to stdout at the wrong physical
        // columns, and the error accumulates across the row — characters
        // from different source cells get smashed into each other's
        // positions, producing the "AtomCodePCodingPlan" / "已添加o4G个"
        // bleed seen on Win11 VSCode pwsh and legacy conhost.
        //
        // Pragmatic fix: keep run-packing for ASCII (which every terminal
        // renders at 1 col, prediction always sound), force a CUP before
        // the next patch on the same row after any non-ASCII cell. Costs
        // ~6 extra bytes per non-ASCII cell — for CJK-heavy frames that's
        // a couple hundred bytes; for ASCII-heavy frames (input box,
        // status row) zero cost.
        //
        // Note: char codepoint, not display width, is the discriminator.
        // U+00D7 (×) is single-byte UTF-8-encodable to 0xC3 0x97 — its
        // codepoint 215 is >= 0x80, putting it on the "force CUP" side.
        if (patch.cell.ch as u32) < 0x80 {
            if let Some((r, c)) = expected_cursor {
                expected_cursor = Some((r, c + patch.cell.width as u16));
            }
        } else {
            expected_cursor = None;
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

/// JediTerm / IntelliJ-terminal repaint: serialise (prev → next) as a
/// per-changed-row **tight** stream, instead of the per-cell-CUP patch
/// stream [`serialize_patches`] produces.
///
/// Why a separate path: [`serialize_patches`] forces an absolute CUP before
/// every non-ASCII cell (see its body) so a width-misprediction degrades to
/// one wrong cell instead of a whole-row bleed. On JediTerm that very
/// CUP-per-glyph is what makes the terminal's narrow-fallback CJK paint show
/// a visible 1-col gap after every ideograph, and turns a rule of `─` into N
/// individually-positioned dashes that paint as a fragmented line.
///
/// JediTerm's GRID agrees with our width model (CJK = 2 cells), so we can
/// safely let the terminal advance its own cursor across a contiguous run.
/// For each row that changed: emit ONE `CUP(row,1)` + `EL` (erase the whole
/// line), then stream the row's cells up to its last non-blank column as a
/// single run — continuation cells skipped, SGR state-machined exactly like
/// [`serialize_row`]. This keeps box-drawing rules contiguous (symptom #3)
/// and the `EL` wipes any stale tail JediTerm left behind (symptom #2). It
/// deliberately does NOT change the per-ideograph gap (symptom #1) — that is
/// a font-advance artifact no escape sequence can fix.
///
/// Right-margin safety: streaming a full-width row writes the last column,
/// which leaves an xterm-class terminal (JediTerm included) in the *deferred*
/// "pending wrap" state — NOT an immediate line break. Every row here begins
/// with an absolute CUP, and `Screen::render_diff` always parks the cursor
/// with a final CUP after these bytes, so the pending wrap is cancelled
/// before any further glyph prints. No auto-wrap into the next row.
pub fn serialize_frames_tight(prev: &[Vec<Cell>], next: &[Vec<Cell>]) -> Vec<u8> {
    let mut out = Vec::new();
    let blank = Cell::blank();
    let max_rows = prev.len().max(next.len());
    for r in 0..max_rows {
        let p = prev.get(r).map(Vec::as_slice).unwrap_or(&[]);
        let n = next.get(r).map(Vec::as_slice).unwrap_or(&[]);
        let max_cols = p.len().max(n.len());
        let changed =
            (0..max_cols).any(|c| p.get(c).unwrap_or(&blank) != n.get(c).unwrap_or(&blank));
        if !changed {
            continue;
        }
        // CUP to (row, col 1) then EL — erase the whole physical line so any
        // stale tail (content that used to extend past the new row's end) is
        // cleared without needing per-column blank patches. 1-indexed.
        let _ = write!(out, "\x1b[{};1H\x1b[K", r + 1);
        // Everything past the last non-blank cell was just cleared by EL and
        // stays blank, so we only re-stream up to there. A row that became
        // all-blank emits nothing further (the EL already cleared it).
        if let Some(last) = n.iter().rposition(|c| c != &blank) {
            let mut current_style: Option<CellStyle> = None;
            let mut emitted_any_sgr = false;
            for cell in &n[..=last] {
                if cell.width == 0 {
                    // Continuation cell — the wide glyph before it already
                    // advanced the terminal cursor past this column.
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
                out.extend_from_slice(cell.ch.encode_utf8(&mut buf).as_bytes());
            }
            if emitted_any_sgr {
                out.extend_from_slice(b"\x1b[0m");
            }
        }
    }
    out
}

/// Serialise a single row of cells into ANSI bytes **without any cursor
/// positioning**. Used by the scrollback-push path (write row to stdout
/// at the current cursor, then let `\n` advance). Skips continuation
/// cells; closes with `\x1b[0m` iff any SGR was emitted so subsequent
/// writes start from a clean state.
pub fn serialize_row(row: &[Cell]) -> Vec<u8> {
    serialize_row_clipped(row, usize::MAX)
}

/// Serialise at most `max_cols` display columns of `row`. Used when
/// the body's effective width must reserve space for an adjacent UI
/// element (e.g. the right-side scrollbar): clipping at the cell
/// boundary is cluster-aware because `cell.width` already encodes
/// wide-glyph extent (continuation cells have width 0). A wide cluster
/// that would straddle the budget is dropped whole, NOT half-painted —
/// without this the trailing scrollbar `│` could land on the second
/// cell of a CJK / emoji cluster and the terminal would render it as
/// mojibake.
pub fn serialize_row_clipped(row: &[Cell], max_cols: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(row.len() * 4);
    let mut current_style: Option<CellStyle> = None;
    let mut emitted_any_sgr = false;
    let mut cols = 0usize;
    for cell in row {
        if cell.width == 0 {
            continue;
        }
        let w = cell.width as usize;
        if cols.saturating_add(w) > max_cols {
            break;
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
        cols += w;
    }
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
    // SGR 22 ("normal intensity") clears both bold AND faint — there is
    // no per-attribute toggle for faint. So a faint→off transition
    // always goes through full reset to avoid clobbering bold state.
    let faint_off = from.faint && !to.faint;
    let fg_change = from.fg != to.fg;
    let bg_change = from.bg != to.bg;

    let needs_reset = bold_off
        || reverse_off
        || faint_off
        || (from.fg.is_some() && to.fg.is_none())
        || (from.bg.is_some() && to.bg.is_none());

    if needs_reset {
        out.extend_from_slice(b"\x1b[0m");
        // After reset, nothing is on — apply `to`'s positive attrs.
        if to.bold {
            out.extend_from_slice(b"\x1b[1m");
        }
        if to.faint {
            out.extend_from_slice(b"\x1b[2m");
        }
        if to.reverse {
            out.extend_from_slice(b"\x1b[7m");
        }
        if let Some(c) = to.fg {
            emit_ansi_color(out, 38, c);
        }
        if let Some(c) = to.bg {
            emit_ansi_color(out, 48, c);
        }
    } else {
        // Additive path — current attributes stay, just flip on whatever
        // `to` adds.
        if !from.bold && to.bold {
            out.extend_from_slice(b"\x1b[1m");
        }
        if !from.faint && to.faint {
            out.extend_from_slice(b"\x1b[2m");
        }
        if !from.reverse && to.reverse {
            out.extend_from_slice(b"\x1b[7m");
        }
        if fg_change {
            if let Some(c) = to.fg {
                emit_ansi_color(out, 38, c);
            } else {
                // Should have been caught by needs_reset, but defensive.
                out.extend_from_slice(b"\x1b[39m");
            }
        }
        if bg_change {
            if let Some(c) = to.bg {
                emit_ansi_color(out, 48, c);
            } else {
                out.extend_from_slice(b"\x1b[49m");
            }
        }
    }
}

/// Serialize a concrete cell color without consulting crossterm's process-global
/// `NO_COLOR` cache. Color enablement has already been resolved into `TerminalCaps`
/// before a `CellStyle` is built; a second ambient-env check here can otherwise turn
/// an explicitly colored cell into `ESC[m` and silently discard its attributes.
fn emit_ansi_color(out: &mut Vec<u8>, channel: u8, color: Color) {
    let reset = if channel == 38 { 39 } else { 49 };
    match color {
        Color::Reset => {
            let _ = write!(out, "\x1b[{reset}m");
        }
        Color::Black => emit_ansi_index(out, channel, 0),
        Color::DarkGrey => emit_ansi_index(out, channel, 8),
        Color::Red => emit_ansi_index(out, channel, 9),
        Color::DarkRed => emit_ansi_index(out, channel, 1),
        Color::Green => emit_ansi_index(out, channel, 10),
        Color::DarkGreen => emit_ansi_index(out, channel, 2),
        Color::Yellow => emit_ansi_index(out, channel, 11),
        Color::DarkYellow => emit_ansi_index(out, channel, 3),
        Color::Blue => emit_ansi_index(out, channel, 12),
        Color::DarkBlue => emit_ansi_index(out, channel, 4),
        Color::Magenta => emit_ansi_index(out, channel, 13),
        Color::DarkMagenta => emit_ansi_index(out, channel, 5),
        Color::Cyan => emit_ansi_index(out, channel, 14),
        Color::DarkCyan => emit_ansi_index(out, channel, 6),
        Color::White => emit_ansi_index(out, channel, 15),
        Color::Grey => emit_ansi_index(out, channel, 7),
        Color::Rgb { r, g, b } => {
            let _ = write!(out, "\x1b[{channel};2;{r};{g};{b}m");
        }
        Color::AnsiValue(value) => emit_ansi_index(out, channel, value),
    }
}

fn emit_ansi_index(out: &mut Vec<u8>, channel: u8, value: u8) {
    let _ = write!(out, "\x1b[{channel};5;{value}m");
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
            faint: false,
            bg: None,
        }
    }

    #[test]
    fn push_str_cells_sgr_handles_256_colour_fg_and_bg() {
        // The modal menu overlay (heatmap, per-model chart) emits
        // `\x1b[38;5;Nm█\x1b[39m`. The parser must decode the extended
        // `38;5;N` / `48;5;N` forms into AnsiValue, not drop them (which
        // left heatmap cells rendering as default-white).
        let mut row: Vec<Cell> = Vec::new();
        push_str_cells_sgr(&mut row, "\x1b[38;5;40m█\x1b[39m", CellStyle::default());
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].style.fg, Some(Color::AnsiValue(40)));

        let mut row2: Vec<Cell> = Vec::new();
        push_str_cells_sgr(&mut row2, "\x1b[48;5;22mX\x1b[49m", CellStyle::default());
        assert_eq!(row2.len(), 1);
        assert_eq!(row2[0].style.bg, Some(Color::AnsiValue(22)));

        // Truecolor `38;2;R;G;B` should also resolve to Rgb.
        let mut row3: Vec<Cell> = Vec::new();
        push_str_cells_sgr(
            &mut row3,
            "\x1b[38;2;10;20;30mY\x1b[0m",
            CellStyle::default(),
        );
        assert_eq!(row3.len(), 1);
        assert_eq!(
            row3[0].style.fg,
            Some(Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            })
        );
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

    /// Uses 0-indexed slice input, produces 1-indexed (row, col)
    /// patches matching ANSI cursor addressing convention.
    #[test]
    fn diff_cell_frames_produces_one_indexed_coords() {
        let row: Vec<Cell> = "ab"
            .chars()
            .map(|ch| Cell {
                ch,
                style: Default::default(),
                width: 1,
            })
            .collect();
        let mut changed = row.clone();
        changed[0].ch = 'X';
        let prev = vec![row.clone()];
        let next = vec![changed];
        let patches = diff_cell_frames(&prev, &next);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].row, 1, "slice row 0 -> ANSI row 1");
        assert_eq!(patches[0].col, 1, "slice col 0 -> ANSI col 1");
        assert_eq!(patches[0].cell.ch, 'X');
    }

    /// Empty frames produce zero patches.
    #[test]
    fn diff_cell_frames_empty_frames() {
        let patches = diff_cell_frames(&[], &[]);
        assert!(patches.is_empty());
    }

    #[test]
    fn diff_shorter_next_emits_blanks_for_trailing() {
        // prev has 5 cells, next has 2 — the 3 tail cells in prev need
        // blanking patches so leftover glyphs get overwritten.
        let prev_row: Vec<Cell> = "hello"
            .chars()
            .map(|ch| Cell {
                ch,
                style: Default::default(),
                width: 1,
            })
            .collect();
        let next_row: Vec<Cell> = "he"
            .chars()
            .map(|ch| Cell {
                ch,
                style: Default::default(),
                width: 1,
            })
            .collect();
        let prev = vec![prev_row];
        let next = vec![next_row];
        let patches = diff_cell_frames(&prev, &next);
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
            cell: Cell {
                ch: 'a',
                style: Default::default(),
                width: 1,
            },
        };
        let p2 = Patch {
            row: 5,
            col: 2,
            cell: Cell {
                ch: 'b',
                style: Default::default(),
                width: 1,
            },
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
            cell: Cell {
                ch: 'a',
                style: Default::default(),
                width: 1,
            },
        };
        let p2 = Patch {
            row: 5,
            col: 2,
            cell: Cell {
                ch: 'b',
                style: CellStyle {
                    fg: None,
                    bold: true,
                    reverse: false,
                    faint: false,
                    bg: None,
                },
                width: 1,
            },
        };
        let bytes = serialize_patches(&[p1, p2]);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\x1b[1m"), "expected bold SGR, got: {:?}", s);
    }

    /// Faint cells emit SGR 2 — theme-aware muting for hint/status text.
    /// Final reset must close the run because faint is "sticky" until
    /// SGR 22 / SGR 0 clears it.
    #[test]
    fn serialize_faint_emits_sgr_two_and_final_reset() {
        let p = Patch {
            row: 1,
            col: 1,
            cell: Cell {
                ch: 'h',
                style: CellStyle {
                    fg: None,
                    bold: false,
                    reverse: false,
                    faint: true,
                    bg: None,
                },
                width: 1,
            },
        };
        let bytes = serialize_patches(std::slice::from_ref(&p));
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\x1b[2m"), "expected faint SGR, got: {:?}", s);
        assert!(s.ends_with("\x1b[0m"), "faint cell must close with reset");
    }

    /// Faint→non-faint transition routes through full reset (SGR 0)
    /// rather than per-attribute toggle, because SGR 22 ("normal
    /// intensity") would also clobber bold if present. Reset path is
    /// what `emit_sgr_transition` already uses for bold-off and
    /// reverse-off — extending to faint-off keeps the invariant.
    #[test]
    fn serialize_faint_off_goes_through_reset() {
        let faint = Patch {
            row: 1,
            col: 1,
            cell: Cell {
                ch: 'a',
                style: CellStyle {
                    fg: None,
                    bold: false,
                    reverse: false,
                    faint: true,
                    bg: None,
                },
                width: 1,
            },
        };
        let plain = Patch {
            row: 1,
            col: 2,
            cell: Cell {
                ch: 'b',
                style: CellStyle::default(),
                width: 1,
            },
        };
        let bytes = serialize_patches(&[faint, plain]);
        let s = String::from_utf8(bytes).unwrap();
        // \x1b[2m for faint cell → \x1b[0m before the plain cell.
        assert!(s.contains("\x1b[2m"));
        // The plain cell must be preceded by a reset, not just \x1b[22m.
        let reset_idx = s
            .match_indices("\x1b[0m")
            .map(|(i, _)| i)
            .find(|&i| i < s.find('b').unwrap())
            .expect("expected mid-stream reset before plain cell");
        let _ = reset_idx;
    }

    // ── Unicode edge-case regression tests ─────────────────────────
    //
    // These pin down how `push_str_cells` / `diff_cells` / `serialize_patches`
    // treat "tricky" Unicode input. For each scenario we either:
    //   (a) assert the behaviour we DO support, or
    //   (b) document a known limitation with `#[ignore]` + a doc
    //       comment explaining what actually happens + why we accept
    //       it for now.
    //
    // Run only this group: `cargo test -p atomcode-tuix --lib unicode_`
    // ──────────────────────────────────────────────────────────────

    /// Baseline: CJK ideographs expand correctly (1 real cell + 1
    /// continuation per char). Covers "你是谁" → 6 cells total,
    /// model col matching terminal col exactly.
    #[test]
    fn unicode_cjk_ideograph_expands_to_two_cells() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "你是谁", &CellStyle::default());
        assert_eq!(row.len(), 6, "3 CJK chars × (1 real + 1 cont) = 6 cells");

        // Real cells carry the glyph + width 2.
        assert_eq!(row[0].ch, '你');
        assert_eq!(row[0].width, 2);
        assert_eq!(row[2].ch, '是');
        assert_eq!(row[2].width, 2);
        assert_eq!(row[4].ch, '谁');
        assert_eq!(row[4].width, 2);

        // Continuation cells: width 0, glyph = space (harmless if
        // something ever did try to serialise them).
        for i in [1, 3, 5] {
            assert_eq!(row[i].width, 0, "cell {} should be continuation", i);
        }
    }

    /// Emoji like 😀 are Unicode "wide" (East Asian Width Wide/Full).
    /// Single code-point emoji should behave like CJK — 1 real + 1
    /// continuation.
    #[test]
    fn unicode_single_codepoint_emoji_expands_to_two_cells() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "😀", &CellStyle::default());
        assert_eq!(row.len(), 2);
        assert_eq!(row[0].ch, '😀');
        assert_eq!(row[0].width, 2);
        assert_eq!(row[1].width, 0);
    }

    /// **Known limitation**: ZWJ-sequence emoji (family, profession,
    /// flag-of-england-style tag sequences, skin-tone modifiers) are
    /// composed of multiple Unicode code points joined by U+200D (ZWJ)
    /// or other joiners. Each code point gets its own `unicode_width`
    /// lookup and our model treats them independently.
    ///
    /// Example: "👨‍👩‍👧" (man + ZWJ + woman + ZWJ + girl)
    ///   - Terminal displays: 1 glyph occupying 2 columns
    ///   - `unicode_width` per codepoint: 2 + 0 + 2 + 0 + 2 = 6
    ///   - Our model: 3 real cells (w=2 each) + 2 skipped (w=0 ZWJ) = 3 real wide glyph cells
    ///   - Total real cells: 3 with width 2 = 6 terminal cols claimed
    ///
    /// If the terminal actually renders the ZWJ sequence as a single
    /// 2-col glyph, our cursor advances 6 while terminal advances 2,
    /// drift = 4. Next character lands 4 cols too far right.
    ///
    /// This test pins down the **current** behaviour so we notice if
    /// we ever silently change it; it doesn't prescribe what's "right"
    /// because the fix requires a grapheme segmenter (unicode-segmentation
    /// crate) that we're not bringing in yet.
    #[test]
    fn unicode_zwj_sequence_is_not_grapheme_aware_known_limitation() {
        let mut row = Vec::new();
        // family emoji: man + ZWJ + woman + ZWJ + girl
        push_str_cells(&mut row, "👨\u{200D}👩\u{200D}👧", &CellStyle::default());
        // 3 wide base chars → 3 real + 3 continuation = 6
        // 2 ZWJ (w=0) → skipped
        // Real cells + continuations only.
        let real_cells = row.iter().filter(|c| c.width > 0).count();
        let cont_cells = row.iter().filter(|c| c.width == 0).count();
        eprintln!(
            "[UNICODE DIAG] ZWJ family: real={} cont={} total={} (terminal would show 1 glyph = 2 cols)",
            real_cells, cont_cells, row.len()
        );
        // Exact counts: 3 real + 3 continuation (ZWJ are width-0, skipped
        // by push_str_cells' early `continue`).
        assert_eq!(real_cells, 3);
        assert_eq!(cont_cells, 3);
        assert_eq!(row.len(), 6);
        // → Known drift: model says 6 cols occupied, terminal shows 2.
    }

    /// **Known limitation**: skin-tone modifiers (👍🏽) — base emoji
    /// U+1F44D followed by Fitzpatrick modifier U+1F3FD. Terminal
    /// typically renders as one 2-col glyph.
    ///
    /// Our model:
    ///   - base: width 2 → 1 real + 1 cont
    ///   - modifier: `unicode_width` returns 2 (wide) → 1 real + 1 cont
    ///   - total: 4 cells, model claims 4 cols; terminal uses 2.
    ///
    /// Drift 2 per skin-toned emoji. Same grapheme-segmenter fix.
    #[test]
    fn unicode_skin_tone_modifier_not_segmented_known_limitation() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "👍🏽", &CellStyle::default());
        let real_cells = row.iter().filter(|c| c.width > 0).count();
        eprintln!(
            "[UNICODE DIAG] skin-tone emoji: real={} cells total={}",
            real_cells,
            row.len()
        );
        // 2 real cells, each advancing cursor by 2 = 4 cols model.
        // Terminal advances 2. Drift = 2.
        assert_eq!(real_cells, 2);
    }

    /// Ambiguous-width chars (East Asian Ambiguous class): `§`, `±`,
    /// `¶`, Greek letters, box-drawing. `unicode-width` crate defaults
    /// these to **1** (narrow); CJK-locale terminals may render them
    /// at **2**. Our model → 1-col advance.
    ///
    /// If your Mac Terminal is set to a CJK language, these chars
    /// will drift left by 1 col per occurrence. Changing `unicode-width`
    /// to ambiguous-as-wide mode is a per-locale decision we don't
    /// attempt; we pin the default behaviour here.
    #[test]
    fn unicode_ambiguous_width_defaults_narrow() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "§±¶", &CellStyle::default());
        // Each char → 1 cell with width 1 (no continuation).
        assert_eq!(row.len(), 3);
        for (i, ch) in "§±¶".chars().enumerate() {
            assert_eq!(row[i].ch, ch);
            assert_eq!(row[i].width, 1);
        }
    }

    /// Combining marks (NFD form): "e + combining acute" = "e\u{301}"
    /// displays as "é" (1 col total), but is **2 code points**. Our
    /// current impl: the combining char has `unicode_width = 0` and
    /// gets dropped by `push_str_cells`' early-continue — the base 'e'
    /// survives unadorned.
    ///
    /// Effect: an NFD-form string in input loses its accent on screen.
    /// NFC-form ("é" U+00E9, single codepoint) renders fine.
    ///
    /// For CJK-dominant usage this is acceptable. Fixing requires
    /// either (a) NFC-normalising input before push, or (b) attaching
    /// combining marks to the preceding cell's char. Neither is in
    /// scope for Ink Phase 1.
    #[test]
    fn unicode_nfd_combining_mark_is_dropped_known_limitation() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "e\u{301}", &CellStyle::default());
        // 'e' kept, combining acute dropped.
        assert_eq!(row.len(), 1, "combining mark dropped by width=0 guard");
        assert_eq!(row[0].ch, 'e');
        assert_eq!(row[0].width, 1);
    }

    /// NFC precomposed accented chars render correctly as narrow cells.
    /// Contrast with `unicode_nfd_combining_mark_is_dropped` above.
    #[test]
    fn unicode_nfc_precomposed_accent_narrow_cell() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "café", &CellStyle::default());
        assert_eq!(row.len(), 4);
        for (i, ch) in "café".chars().enumerate() {
            assert_eq!(row[i].ch, ch);
            assert_eq!(row[i].width, 1);
        }
    }

    /// Zero-width space (U+200B) and BOM (U+FEFF) are width-0 and
    /// legitimately get dropped — they carry no glyph. This verifies
    /// the width=0 guard does the right thing rather than accidentally
    /// inserting a phantom cell that would misalign diff col numbers.
    #[test]
    fn unicode_zero_width_invisibles_dropped() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "a\u{200B}b\u{FEFF}c", &CellStyle::default());
        // a, b, c — invisibles silently dropped.
        assert_eq!(row.len(), 3);
        assert_eq!(row[0].ch, 'a');
        assert_eq!(row[1].ch, 'b');
        assert_eq!(row[2].ch, 'c');
    }

    /// Mixed-width input: the `cell_index == terminal_column`
    /// invariant holds across narrow + wide alternation.
    #[test]
    fn unicode_mixed_width_cell_indices_match_terminal_cols() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "a你b", &CellStyle::default());
        // Expect: 'a'(w=1) + '你'(w=2) + cont(w=0) + 'b'(w=1) = 4 cells
        assert_eq!(row.len(), 4);
        // Verify terminal-col calculation: cell i represents col i+1.
        // If cursor_advance = cell.width summed, total = 1+2+0+1 = 4,
        // matching terminal "a你b" = 1+2+1 = 4 cols.
        let total_advance: u16 = row.iter().map(|c| c.width as u16).sum();
        assert_eq!(total_advance, 4);
    }

    /// Diff + serialize round-trip with wide chars — a narrow→wide
    /// transition at the same cell position must emit the wide glyph
    /// AND leave the continuation position "covered" so subsequent
    /// writes don't overwrite the glyph's right half.
    #[test]
    fn unicode_diff_narrow_to_wide_at_same_position() {
        let prev_row: Vec<Cell> = vec![
            Cell {
                ch: 'a',
                style: CellStyle::default(),
                width: 1,
            },
            Cell {
                ch: 'b',
                style: CellStyle::default(),
                width: 1,
            },
        ];
        let next_row: Vec<Cell> = vec![
            Cell {
                ch: '你',
                style: CellStyle::default(),
                width: 2,
            },
            Cell::continuation(),
        ];
        // Row 9 in the slice (index) → ANSI row 10 in the patch.
        let prev: Vec<Vec<Cell>> = (0..9)
            .map(|_| Vec::new())
            .chain(std::iter::once(prev_row))
            .collect();
        let next: Vec<Vec<Cell>> = (0..9)
            .map(|_| Vec::new())
            .chain(std::iter::once(next_row))
            .collect();

        let patches = diff_cell_frames(&prev, &next);
        assert_eq!(patches.len(), 2, "both cols changed");

        let bytes = serialize_patches(&patches);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains('你'));
        // Exactly one cursor-position CSI (the initial move).
        assert_eq!(
            s.matches("\x1b[10;").count(),
            1,
            "continuation must not trigger a cursor move: {:?}",
            s
        );
    }

    // ── serialize_frames_tight (JediTerm per-row tight repaint) ──────
    //
    // These pin the byte-stream contract of the JediTerm path: one CUP +
    // EL per changed row, then a single contiguous run (no per-cell CUP),
    // continuation cells skipped, stale tails cleared. Contrast with
    // `serialize_patches` which forces a CUP before every non-ASCII cell.

    #[test]
    fn tight_emits_rule_as_single_contiguous_run() {
        // 5 box-drawing dashes (each non-ASCII). serialize_patches would
        // CUP each (5 cursor moves); tight emits ONE CUP then streams all 5.
        let mut rule = Vec::new();
        push_str_cells(&mut rule, "─────", &CellStyle::default());
        let prev = vec![vec![Cell::blank(); 5]];
        let next = vec![rule];
        let s = String::from_utf8(serialize_frames_tight(&prev, &next)).unwrap();
        assert!(
            s.starts_with("\x1b[1;1H\x1b[K"),
            "row opens with CUP+EL: {:?}",
            s
        );
        // 'H' is the final byte of a CUP; exactly one for the whole row.
        assert_eq!(s.matches('H').count(), 1, "exactly one CUP, got: {:?}", s);
        assert!(s.contains("─────"), "dashes must be contiguous: {:?}", s);
    }

    #[test]
    fn tight_unchanged_row_emits_nothing() {
        let mut row = Vec::new();
        push_str_cells(&mut row, "hi", &CellStyle::default());
        let prev = vec![row.clone()];
        let next = vec![row];
        assert!(serialize_frames_tight(&prev, &next).is_empty());
    }

    #[test]
    fn tight_clears_tail_when_row_shrinks() {
        // prev "hello" → next "he": the EL must wipe the "llo" tail and we
        // re-stream only "he" (never re-emit the stale tail).
        let prev_row: Vec<Cell> = "hello"
            .chars()
            .map(|ch| Cell {
                ch,
                style: CellStyle::default(),
                width: 1,
            })
            .collect();
        let next_row: Vec<Cell> = "he"
            .chars()
            .map(|ch| Cell {
                ch,
                style: CellStyle::default(),
                width: 1,
            })
            .collect();
        let s =
            String::from_utf8(serialize_frames_tight(&vec![prev_row], &vec![next_row])).unwrap();
        assert!(
            s.contains("\x1b[1;1H\x1b[K"),
            "row cleared with EL: {:?}",
            s
        );
        assert!(s.contains("he"));
        assert!(
            !s.contains("llo"),
            "stale tail must not be re-emitted: {:?}",
            s
        );
    }

    #[test]
    fn tight_skips_continuation_and_emits_cjk_glyph_once() {
        // "a你b": 你 is width 2 + a width-0 continuation cell that must be
        // skipped (like serialize_patches) — the run stays "a你b".
        let mut row = Vec::new();
        push_str_cells(&mut row, "a你b", &CellStyle::default());
        let prev = vec![vec![Cell::blank(); row.len()]];
        let next = vec![row];
        let s = String::from_utf8(serialize_frames_tight(&prev, &next)).unwrap();
        assert!(
            s.contains("a你b"),
            "glyph emitted once, no phantom cont: {:?}",
            s
        );
        assert_eq!(s.matches('H').count(), 1, "single CUP for the row: {:?}", s);
    }

    #[test]
    fn tight_full_blank_row_just_clears() {
        // Row went from content to all-blank → only CUP+EL, nothing streamed.
        let prev_row: Vec<Cell> = "xy"
            .chars()
            .map(|ch| Cell {
                ch,
                style: CellStyle::default(),
                width: 1,
            })
            .collect();
        let next = vec![vec![Cell::blank(); 2]];
        let s = String::from_utf8(serialize_frames_tight(&vec![prev_row], &next)).unwrap();
        assert_eq!(
            s, "\x1b[1;1H\x1b[K",
            "blank row emits only the clear: {:?}",
            s
        );
    }

    /// Reverse: wide→narrow at same cell index. The wide char's
    /// right-half column needs an explicit blank patch to erase the
    /// stale glyph half left over after overwriting the left half.
    #[test]
    fn unicode_diff_wide_to_narrow_erases_right_half() {
        let prev_row: Vec<Cell> = vec![
            Cell {
                ch: '你',
                style: CellStyle::default(),
                width: 2,
            },
            Cell::continuation(),
        ];
        let next_row: Vec<Cell> = vec![
            Cell {
                ch: 'a',
                style: CellStyle::default(),
                width: 1,
            },
            Cell {
                ch: 'b',
                style: CellStyle::default(),
                width: 1,
            },
        ];
        let prev: Vec<Vec<Cell>> = (0..4)
            .map(|_| Vec::new())
            .chain(std::iter::once(prev_row))
            .collect();
        let next: Vec<Vec<Cell>> = (0..4)
            .map(|_| Vec::new())
            .chain(std::iter::once(next_row))
            .collect();

        let patches = diff_cell_frames(&prev, &next);
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].col, 1);
        assert_eq!(patches[0].cell.ch, 'a');
        assert_eq!(patches[1].col, 2);
        assert_eq!(patches[1].cell.ch, 'b');

        let bytes = serialize_patches(&patches);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains('a'));
        assert!(s.contains('b'));
    }
}
