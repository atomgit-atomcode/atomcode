// crates/atomcode-tuix/src/markdown.rs
//
// Line-oriented markdown renderer. Handles:
//   **bold** / *italic* / `code` (inline)
//   # / ## / ### headings
//   - / * bullet lists
//   ```fenced code blocks``` (state-tracked)
//   --- horizontal rules
// Tables are passed through as raw text (pipes show literally).

use crate::highlight::theme;
use crate::terminal::TerminalCaps;
use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;

/// Parser state maintained across lines of a streamed response.
#[derive(Clone)]
pub struct MdState {
    pub in_code_block: bool,
    /// The fence character that opened the current block (`'`' or `'~'`).
    /// Used so that `~~~` inside a backtick block is not mistaken for a
    /// close fence (issue #699).
    fence_char: char,
    /// Number of consecutive markers in the opening fence. A close fence
    /// must have at least this many markers so that ```` ``` ```` (4-backtick
    /// block) can safely contain ```` ``` ```` (3-backtick inner blocks).
    fence_len: usize,
    /// Accumulates consecutive `|…|` rows; flushed as an aligned block
    /// when a non-table line arrives.
    pub table_buf: Vec<String>,
    /// Lines accumulated between an opening and closing code fence.
    /// Flushed through `highlight::highlight_block` on close fence so
    /// the code block appears in one chunk at fence close rather than
    /// streaming line-by-line.
    pub code_buf: Vec<String>,
    /// Raw source of the most recently closed code block (the original
    /// lines joined with `\n`, before the 2-space indent from
    /// `highlight_block`). Set on close-fence and unclosed-finalize
    /// paths. Callers take the value via `Option::take()` and push it
    /// to the system clipboard via arboard / OSC 52 so the user can
    /// paste the unwrapped original instead of selecting wrapped display
    /// lines (issue #699). `None` means no code block was flushed this
    /// tick.
    ///
    /// Held until the next close fence or explicit `take()` — for very
    /// large single-block replies the peak memory is `O(source size)`
    /// which matches the existing `code_buf` behaviour.
    pub last_code_block_source: Option<String>,
    /// How many properly-CLOSED fenced code blocks have been seen since the
    /// last [`reset`](Self::reset) (i.e. in the current assistant message).
    /// Auto-copy fires ONLY when a whole reply is essentially a SINGLE code
    /// block (`== 1`): a long answer that merely contains several illustrative
    /// blocks must not clobber the clipboard per-block nor spam a "copied" hint.
    /// An UNCLOSED / prematurely-finalized block does NOT increment this — only
    /// a real close fence does — so an incomplete block is never auto-copied.
    pub code_block_count: usize,
    /// Trimmed text of the most recently rendered heading, used to DROP a
    /// consecutive byte-identical repeat. Weak models sometimes emit the same
    /// `## Section` twice with only a table (or blank line) between — a rendering
    /// artefact the user reported. It persists across buffered tables and blank
    /// lines but is CLEARED by any real content (prose / list / code), so a
    /// heading legitimately reused in a later section (with content between) is
    /// never suppressed. `None` = no heading since the last content reset.
    pub last_heading: Option<String>,
}

/// Manual impl so `fence_char` / `fence_len` start at realistic sentinel
/// values instead of `\0` / `0` (what `#[derive(Default)]` would give).
/// These fields are only read when `in_code_block` is true, which always
/// follows an explicit set in the OPEN-fence branch, but explicit defaults
/// make the invariant visible and protect against future refactors.
impl Default for MdState {
    fn default() -> Self {
        Self {
            in_code_block: false,
            fence_char: '`',
            fence_len: 3,
            table_buf: Vec::new(),
            code_buf: Vec::new(),
            last_code_block_source: None,
            code_block_count: 0,
            last_heading: None,
        }
    }
}

impl MdState {
    pub fn new() -> Self {
        Self::default()
    }
    /// Reset all fields to their initial state.  Keep in sync with
    /// [`Default::default()`] — any new field added to `MdState` must
    /// be cleared here too.
    pub fn reset(&mut self) {
        self.in_code_block = false;
        self.fence_char = '`';
        self.fence_len = 3;
        self.table_buf.clear();
        self.code_buf.clear();
        self.last_code_block_source = None;
        self.code_block_count = 0;
        self.last_heading = None;
    }
}

/// Render one complete line with block- and inline-level markdown applied.
/// Returns None if the line should be omitted from output (e.g., a fence
/// marker ``` that toggles code-block state but isn't itself visible text).
pub fn render_line(line: &str, state: &mut MdState, caps: TerminalCaps) -> Option<String> {
    render_line_with_width(line, state, caps, 0)
}

/// Width-aware variant of [`render_line`]. When `max_width > 0`, a flushed
/// table's column widths are capped so every line fits the budget — otherwise
/// `wrap_cells_to_width` downstream chops long rows and shatters the table's
/// border structure. `max_width = 0` keeps legacy behaviour.
pub fn render_line_with_width(
    line: &str,
    state: &mut MdState,
    caps: TerminalCaps,
    max_width: usize,
) -> Option<String> {
    let trimmed = line.trim();

    // Table row: buffer and defer emit until the block ends. GitHub-flavored
    // markdown allows tables WITHOUT the leading/trailing `|` (`a | b` +
    // `---|---` is as valid as `| a | b |`), and models emit both forms. Keying
    // detection on `starts_with('|')` missed the pipe-less form entirely — those
    // tables fell through and rendered as raw markdown source (issue: long
    // comparison tables shown un-rendered). Buffer any row that splits into ≥2
    // cells (respecting code-span pipes + `\|` via `split_table_row`); the flush
    // then REQUIRES a delimiter row, so a lone prose line with a literal `|`
    // still renders as prose, not a box.
    //
    // EXCLUDE list items: a bullet/number line that merely mentions a pipe
    // (`- option A | option B`, `1. run cmd a | cmd b`) is a LIST item, not a
    // table row — GFM only treats it as a table cell when a table is actually
    // established. Without this guard the broadened detection stole such items
    // from the list path and dropped their marker. The `---|---` delimiter is
    // NOT a list item (`- ` needs a trailing space), so tables still work.
    if !state.in_code_block
        && split_table_row(trimmed).len() >= 2
        && parse_list_item(line).is_none()
    {
        state.table_buf.push(trimmed.to_string());
        return None;
    }

    // Pre-drawn Unicode box-drawing table row (`┌─┬─┐ │ ├─┼─┤ └─┴─┘`).
    // Some models — usually weaker ones mimicking earlier-turn output that
    // we ourselves rendered — emit tables fully drawn in box characters
    // instead of `|`-form markdown. Without detection, those rows fall
    // through to the inline-only branch and `push_markdown_body`'s
    // wrap-at-cell-level chops them at terminal width, shattering the
    // borders (the macOS overflow case in the screenshot). Convert each
    // row to the equivalent pipe form (│ → |, ─ → -, junctions → |) and
    // route through the same buffer + flush path the `|`-form takes;
    // `flush_aligned_table_with_width` then enforces flat-mode fallback
    // for narrow terminals exactly like a real markdown table would get.
    if !state.in_code_block {
        if let Some(converted) = box_drawing_table_row(trimmed) {
            state.table_buf.push(converted);
            return None;
        }
    }

    // Non-table line arriving after buffered rows: flush as aligned block.
    let prefix = if !state.table_buf.is_empty() {
        let t = flush_aligned_table_with_width(&state.table_buf, caps, max_width);
        state.table_buf.clear();
        Some(t)
    } else {
        None
    };
    let prepend = |body: String| -> String {
        match prefix.as_ref() {
            Some(p) => format!("{}\n{}", p, body),
            None => body,
        }
    };
    let prefix_only = || -> Option<String> { prefix.as_ref().map(|p| p.clone()) };

    // Fenced code block fence (``` or ~~~).
    //
    // Two separate checks — the close fence must match the opening
    // marker character and be at least as long, so a 4-backtick block
    // can safely contain a 3-backtick inner block, and `~~~` inside a
    // backtick block is NOT mistaken for a close fence (issue #699 P1-2).
    //
    // CLOSE fence: flush the buffered block through `highlight::highlight_block`.
    if state.in_code_block && is_closing_fence(trimmed, state.fence_char, state.fence_len) {
        let source = state.code_buf.join("\n");
        let highlighted = crate::highlight::highlight_block(&source);
        state.last_code_block_source = Some(source);
        // A PROPERLY-CLOSED block — the only kind eligible for auto-copy.
        state.code_block_count += 1;
        state.in_code_block = false;
        state.code_buf.clear();
        return Some(prepend(highlighted));
    }
    // OPEN fence: at least 3 consecutive backticks or tildes, captured
    // so the close check above can match the marker and count.
    if !state.in_code_block {
        if let Some((c, len)) = fence_start(trimmed) {
            state.in_code_block = true;
            state.fence_char = c;
            state.fence_len = len;
            state.code_buf.clear();
            state.last_heading = None; // a code block is real content
            return prefix_only();
        }
    }

    // Inside code block: buffer the line, defer rendering until close fence.
    // No per-line output; the highlighter needs full context.
    if state.in_code_block {
        state.code_buf.push(line.to_string());
        return prefix_only();
    }

    // Horizontal rule — render as a blank separator line, not a visible
    // rule. A horizontal bar overwhelms the surrounding prose; a blank line
    // communicates the same thematic break far more gracefully.
    if is_hrule(trimmed) {
        return Some(prepend(String::new()));
    }

    // Heading — H1-H3 get bold + bright cyan (Palette::ACCENT, SGR 96)
    // so headings sit on their own colour layer above the default-colour
    // body. Bright cyan was chosen over bright magenta (BRAND, 95)
    // because terminals that remap bright white (97, used by inline code
    // and code blocks) to lavender — Catppuccin / Tokyo Night / similar
    // — typically remap bright magenta to the same lavender, which
    // would collapse heading colour into the inline-code colour.
    // Cyan stays hue-distinct on those palettes and on plain ANSI.
    // H4+ keeps italic-only so the deep-hierarchy levels still read as
    // "weaker than a real heading" without adding a third colour tier.
    if let Some((level, rest)) = parse_heading(line) {
        // DROP a consecutive byte-identical heading (only tables/blanks between).
        // Flush any buffered table first so its content is never lost — only the
        // duplicated heading line itself is skipped.
        if state.last_heading.as_deref() == Some(trimmed) {
            return prefix_only();
        }
        state.last_heading = Some(trimmed.to_string());
        let inner = render_inline(rest, caps);
        let body = if !caps.colors {
            format!("{} {}", "#".repeat(level as usize), inner)
        } else {
            match level {
                1 | 2 | 3 => format!(
                    "{}{}{}",
                    theme::md_heading_open(),
                    inner,
                    theme::MD_HEADING_CLOSE
                ),
                _ => format!(
                    "{}{}{}",
                    theme::MD_ITALIC_OPEN,
                    inner,
                    theme::MD_ITALIC_CLOSE
                ),
            }
        };
        return Some(prepend(body));
    }

    // List (unordered or ordered): `- text` / `* text` / `1. text`
    // Marker (• / 1.) rendered in muted gray so it sits quietly next to
    // the default-fg body text — visually distinct without adding another
    // bright colour tier. The space after the marker keeps readability.
    // Theme-aware: dark themes use SGR 37 (soft light-gray) because the
    // fixed SGR 90 collapses to ~3:1 against dark backgrounds and the
    // marker is invisible until selected (Issue #1426) — same fix table
    // borders already had via `md_border_open`.
    if let Some(item) = parse_list_item(line) {
        state.last_heading = None; // real content ends the dedup window
        let inner = render_inline(&item.rest, caps);
        let indent = " ".repeat(item.indent);
        let body = if caps.colors {
            format!(
                "{}{}{}{}{}",
                indent,
                theme::md_marker_open(),
                item.marker,
                theme::MD_MUTED_CLOSE,
                inner
            )
        } else {
            format!("{}{} {}", indent, item.marker, inner)
        };
        return Some(prepend(body));
    }

    // Default: inline-only. Non-blank prose ends the dedup window (a heading
    // reused after real content is legitimate); a blank line does NOT, so two
    // identical headings separated only by whitespace still de-duplicate.
    if !trimmed.is_empty() {
        state.last_heading = None;
    }
    Some(prepend(render_inline(line, caps)))
}

/// Emit any still-buffered block (e.g., a table that ended without a
/// following non-table line). Call at stream end.
pub fn finalize(state: &mut MdState, caps: TerminalCaps) -> Option<String> {
    finalize_with_width(state, caps, 0)
}

/// Width-aware variant of [`finalize`]. See [`render_line_with_width`].
pub fn finalize_with_width(
    state: &mut MdState,
    caps: TerminalCaps,
    max_width: usize,
) -> Option<String> {
    // Two independent buffers can be open at stream end: a table waiting
    // for a separator row, or a code block whose close fence never came.
    // Both must be emitted so the user doesn't lose content.
    let table_part = if !state.table_buf.is_empty() {
        let t = flush_aligned_table_with_width(&state.table_buf, caps, max_width);
        state.table_buf.clear();
        Some(t)
    } else {
        None
    };

    let code_part = if state.in_code_block && !state.code_buf.is_empty() {
        // An UNCLOSED block reaching finalize — either the model never wrote the
        // closing ```, or a mid-stream flush (e.g. a tool call interleaving the
        // reply) forced an early finalize. Render it so no content is lost, but
        // do NOT feed auto-copy: an incomplete block must never be copied, and a
        // premature mid-stream finalize must not fire a spurious "copied" hint.
        // (Only the real close-fence path sets `last_code_block_source` / bumps
        // `code_block_count`.)
        let source = state.code_buf.join("\n");
        let highlighted = crate::highlight::highlight_block(&source);
        state.in_code_block = false;
        state.code_buf.clear();
        Some(highlighted)
    } else {
        None
    };

    match (table_part, code_part) {
        (None, None) => None,
        (Some(t), None) => Some(t),
        (None, Some(c)) => Some(c),
        (Some(t), Some(c)) => Some(format!("{}\n{}", t, c)),
    }
}

/// Recognise a pre-drawn Unicode box-drawing table line and return the
/// equivalent `|`-pipe form so it can join the same buffering path as
/// real markdown tables. Returns None for lines that aren't part of a box
/// table.
///
/// Two row shapes accepted:
///   1. **Data row** — starts with `│`. Each `│` becomes `|`; cell content
///      passes through unchanged. Caller buffers the result and the
///      existing flush logic splits on `|` and trims as usual.
///   2. **Border row** — starts with `┌`/`├`/`└` AND every char is in the
///      box-drawing set (`─┌┬┐├┼┤└┴┘`) plus spaces. Junctions become `|`
///      and `─` becomes `-`, producing a `|---|---|`-style separator that
///      `flush_aligned_table_with_width`'s `is_sep` matcher already
///      recognises (its predicate is `[-: ]+` per cell).
///
/// The "every char is box-drawing" guard on border rows defends against
/// false positives: a stray paragraph that happens to begin with `├` for
/// some unrelated reason would NOT match (it has letters too).
fn box_drawing_table_row(trimmed: &str) -> Option<String> {
    let first = trimmed.chars().next()?;
    match first {
        '│' => Some(trimmed.replace('│', "|")),
        '┌' | '├' | '└' => {
            if trimmed.chars().all(|c| {
                matches!(
                    c,
                    '─' | '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘' | ' '
                )
            }) {
                let converted: String = trimmed
                    .chars()
                    .map(|c| match c {
                        '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘' => {
                            '|'
                        }
                        '─' => '-',
                        other => other,
                    })
                    .collect();
                Some(converted)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Flush a buffered markdown table as a column-aligned block. Computes the
/// max display width per column and pads every cell accordingly. Columns use
/// whitespace gutters and ASCII separators, avoiding ambiguous-width Unicode
/// borders on Windows console hosts. Inline markdown inside cells is honoured.
pub fn flush_aligned_table(rows: &[String], caps: TerminalCaps) -> String {
    flush_aligned_table_with_width(rows, caps, 0)
}

/// Split a markdown table row on `|`, honouring:
///   * `` ` ``…`` ` `` inline code spans (pipes inside are literal, not
///     separators — so Rust closures `|a, b|`, pattern alternatives
///     `Foo | Bar`, bash pipes `cat | grep`, Python type unions
///     `int | str` etc. inside backticks stay in one cell)
///   * `\|` escape (standard markdown escape for a literal pipe in
///     a cell)
///   * Leading/trailing `|` delimiters (stripped so the edges aren't
///     turned into empty cells)
///
/// Returns trimmed cell strings with backticks preserved (downstream
/// inline formatter handles them). `\|` outside a code span is
/// rewritten to a literal `|` in the cell so the renderer shows the
/// pipe glyph without the backslash.
fn split_table_row(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut cells: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_code = false;
    let mut i = 0;
    if !bytes.is_empty() && bytes[0] == b'|' {
        i = 1;
    }
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'`' {
            in_code = !in_code;
            current.push('`');
            i += 1;
            continue;
        }
        if !in_code && b == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            current.push('|');
            i += 2;
            continue;
        }
        if !in_code && b == b'|' {
            let rest = &line[i + 1..];
            if rest.trim().is_empty() {
                cells.push(current.trim().to_string());
                current = String::new();
                break;
            }
            cells.push(current.trim().to_string());
            current = String::new();
            i += 1;
            continue;
        }
        let ch_len = if b < 128 {
            1
        } else {
            let mut len = 1;
            while i + len < bytes.len() && (bytes[i + len] & 0xc0) == 0x80 {
                len += 1;
            }
            len
        };
        current.push_str(&line[i..i + ch_len]);
        i += ch_len;
    }
    if !current.is_empty() || cells.is_empty() {
        cells.push(current.trim().to_string());
    }
    cells
}

/// A GFM delimiter row (`---|:--:|--:`) — cells are made up only of `-`, `:`
/// and spaces. Shared by every table stage so the "is this the separator?"
/// rule can't drift between the aligned-grid and flat paths.
fn is_separator_row(row: &[String]) -> bool {
    row.iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
}

/// GFM-effective column count: the widest NON-separator (content) row, minus any
/// TRAILING columns that are empty across every content row. Weak models often
/// emit a stray extra `|` or an over-long delimiter (`---|---|---|---`); counting
/// columns as the raw max-over-all-rows (separator included) then painted a ghost
/// empty column on the right. Trimming trailing all-empty columns — and ignoring
/// separator-only width — keeps the drawn grid to the table's real shape. Always
/// keeps at least one column.
fn effective_ncols(parsed: &[Vec<String>]) -> usize {
    let content: Vec<&Vec<String>> = parsed.iter().filter(|r| !is_separator_row(r)).collect();
    let mut n = content.iter().map(|r| r.len()).max().unwrap_or(0);
    while n > 1
        && content
            .iter()
            .all(|r| r.get(n - 1).map_or(true, |c| c.trim().is_empty()))
    {
        n -= 1;
    }
    n
}

/// Display width of the longest whitespace-delimited token in `plain`. Used as
/// a column's shrink floor: a column should not be squeezed narrower than its
/// widest unbreakable word, or the word gets char-wrapped mid-token. Runs of
/// CJK (no interior spaces) count as one token — but the caller caps the floor,
/// and CJK reads fine char-wrapped, so that's fine.
fn longest_token_width(plain: &str) -> usize {
    plain
        .split_whitespace()
        .map(crate::width::display_width)
        .max()
        .unwrap_or(0)
}

/// Word-aware wrap of plain cell text to `width` display columns: greedily pack
/// whitespace-delimited tokens per line, and char-wrap (via
/// [`crate::width::wrap_line_to_width`]) only a single token too wide to fit —
/// so prose breaks at spaces (`Handles login and` / `session tokens`) instead of
/// mid-word, while a long identifier or a space-less CJK run still degrades
/// gracefully. Returns at least one (possibly empty) line.
fn word_wrap_cell(plain: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![plain.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for token in plain.split_whitespace() {
        let tw = crate::width::display_width(token);
        if tw > width {
            // Token can't fit on any line — char-wrap it. Flush what we have,
            // emit the full chunks, and keep the trailing partial as the new
            // current line so following short tokens can pack onto it.
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            let mut chunks = crate::width::wrap_line_to_width(token, width);
            if let Some(last) = chunks.pop() {
                lines.extend(chunks);
                cur_w = crate::width::display_width(&last);
                cur = last;
            }
            continue;
        }
        let sep = usize::from(!cur.is_empty());
        if cur_w + sep + tw > width {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(token);
            cur_w = tw;
        } else {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(token);
            cur_w += sep + tw;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Middle tier between the natural grid and the flat key/value fallback: try to
/// keep the GRID by shrinking wide columns (wrapping their cells to multiple
/// lines) so a table that's only *somewhat* too wide doesn't collapse straight
/// to a vertical list. Returns per-column widths that fit `max_width` (gutters
/// + padding included), or `None` when even at each column's floor the row
/// can't fit — then the caller uses the flat fallback.
/// Column floors clamp to `[MIN_COL, TOKEN_CAP]` around each
/// column's longest token so identifiers aren't broken mid-word, while a single
/// enormous token can't veto the whole grid.
const TABLE_COLUMN_GAP: usize = 2;
const TABLE_CELL_PADDING: usize = 1;
const TABLE_RIGHT_GUARD: usize = 1;

fn fit_columns_to_grid(
    parsed: &[Vec<String>],
    natural: &[usize],
    ncols: usize,
    max_width: usize,
) -> Option<Vec<usize>> {
    const MIN_COL: usize = 4;
    const TOKEN_CAP: usize = 16;
    // Codex-style borderless layout: one-cell padding on both sides of each
    // value, plus a two-cell gap between columns. Avoiding vertical Unicode
    // borders is deliberate: PowerShell/ConPTY/font combinations disagree on
    // the width of East Asian Ambiguous box characters, and the error used to
    // accumulate once per column until the row crossed the terminal edge.
    let chrome = TABLE_CELL_PADDING * 2 * ncols + TABLE_COLUMN_GAP * ncols.saturating_sub(1);
    let content_budget = max_width.checked_sub(chrome)?;
    if content_budget == 0 {
        return None;
    }
    let mut floors = vec![MIN_COL; ncols];
    for row in parsed {
        if is_separator_row(row) {
            continue;
        }
        for (j, cell) in row.iter().enumerate() {
            if j >= ncols {
                break;
            }
            let tok = longest_token_width(&strip_md_for_width(cell));
            floors[j] = floors[j].max(tok.min(TOKEN_CAP));
        }
    }
    // Never grow beyond natural — a column narrower than its floor stays put.
    for (f, n) in floors.iter_mut().zip(natural) {
        *f = (*f).min(*n);
    }
    if floors.iter().sum::<usize>() > content_budget {
        return None;
    }
    // Greedily shave the widest still-shrinkable column until the row fits.
    let mut w = natural.to_vec();
    let mut total: usize = w.iter().sum();
    while total > content_budget {
        let mut best: Option<usize> = None;
        for j in 0..ncols {
            if w[j] > floors[j] && best.map_or(true, |b| w[j] > w[b]) {
                best = Some(j);
            }
        }
        match best {
            Some(j) => {
                w[j] -= 1;
                total -= 1;
            }
            // Unreachable: floor_sum ≤ budget guarantees we can reach it.
            None => return None,
        }
    }
    Some(w)
}

/// True when the wrapped grid would be so cramped that a flat key/value record
/// reads better. Mirrors codex's "starved cell" fallback: fragmented tokens,
/// several cramped cells, or catastrophically tall rows count against the grid,
/// while one exceptional row does not flatten an otherwise useful large table.
fn grid_too_starved(parsed: &[Vec<String>], col_widths: &[usize]) -> bool {
    const MIN_SCANNABLE_WIDTH: usize = 12;
    const CRAMPED_CELL_LINES: usize = 4;
    const CATASTROPHIC_CELL_LINES: usize = 7;

    // The first non-separator row is the header; judge readability from body
    // records only. One affected record is enough for a one-row table. For a
    // larger table, require either two rows or one third of the records so one
    // exceptional cell does not unnecessarily flatten an otherwise useful grid.
    let body_rows: Vec<&Vec<String>> = parsed
        .iter()
        .filter(|row| !is_separator_row(row))
        .skip(1)
        .collect();
    if body_rows.is_empty() {
        return false;
    }

    let affected = body_rows
        .iter()
        .filter(|row| {
            let mut cramped_cells = 0usize;
            let mut catastrophic = false;
            let mut fragmented_token = false;
            let mut row_height = 1usize;
            for (j, &width) in col_widths.iter().enumerate() {
                let cell = row.get(j).map(|s| s.as_str()).unwrap_or("");
                let plain = strip_md_for_width(cell);
                let height = word_wrap_cell(&plain, width).len();
                row_height = row_height.max(height);
                cramped_cells += usize::from(height >= CRAMPED_CELL_LINES);
                catastrophic |= width < MIN_SCANNABLE_WIDTH && height >= CATASTROPHIC_CELL_LINES;
                fragmented_token |= width < MIN_SCANNABLE_WIDTH
                    && plain
                        .split_whitespace()
                        .any(|token| crate::width::display_width(token) > width);
            }
            fragmented_token || cramped_cells >= 2 || catastrophic || row_height > 8
        })
        .count();
    let threshold = if body_rows.len() == 1 {
        1
    } else {
        2.max(body_rows.len().div_ceil(3))
    };
    affected >= threshold
}

/// Render the wrapped-grid middle tier: a borderless table whose cells wrap to their
/// (shrunk) column widths. Cell content is rendered as PLAIN text (inline
/// styling dropped) in this degraded mode — wrapping styled SGR spans across a
/// line boundary would desync the per-line padding and break column alignment.
/// The gain is that no content is lost and the scannable grid survives.
fn render_wrapped_table(
    parsed: &[Vec<String>],
    col_widths: &[usize],
    caps: TerminalCaps,
) -> String {
    let ncols = col_widths.len();
    let data_rows: Vec<&Vec<String>> = parsed.iter().filter(|r| !is_separator_row(r)).collect();
    let mut out = String::new();
    for (i, row) in data_rows.iter().enumerate() {
        // Wrap every cell to its column width; row height = tallest cell.
        let wrapped: Vec<Vec<String>> = (0..ncols)
            .map(|j| {
                let cell = row.get(j).map(|s| s.as_str()).unwrap_or("");
                word_wrap_cell(&strip_md_for_width(cell), col_widths[j])
            })
            .collect();
        let height = wrapped.iter().map(|c| c.len()).max().unwrap_or(1).max(1);
        for line_idx in 0..height {
            for (j, w) in col_widths.iter().enumerate() {
                let cell_line = wrapped[j].get(line_idx).map(|s| s.as_str()).unwrap_or("");
                let lw = crate::width::display_width(cell_line);
                out.push_str(&" ".repeat(TABLE_CELL_PADDING));
                if i == 0 && caps.colors {
                    out.push_str(theme::md_heading_open());
                }
                out.push_str(cell_line);
                if i == 0 && caps.colors {
                    out.push_str(theme::MD_HEADING_CLOSE);
                }
                for _ in 0..w.saturating_sub(lw) {
                    out.push(' ');
                }
                out.push_str(&" ".repeat(TABLE_CELL_PADDING));
                if j + 1 < ncols {
                    out.push_str(&" ".repeat(TABLE_COLUMN_GAP));
                }
            }
            out.push('\n');
        }
        if i + 1 < data_rows.len() {
            out.push_str(&render_table_separator(
                col_widths,
                if i == 0 { '━' } else { '─' },
                caps,
            ));
            out.push('\n');
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Low-contrast color wrapper for table separator lines — borderless aligned-column
/// table, segmented low-contrast rules. Returns `(open_seq, close_seq)`.
///
/// Modern 256-color terminals get a 256-color soft-gray that retreats to the
/// background: `AnsiValue(240)` on dark themes, `AnsiValue(250)` on light themes.
/// Legacy 16-color terminals get SGR 2 (faint), which most terminals render as a
/// dimmed version of the default foreground.
/// Low-contrast wrap for table rules: FAINT (SGR 2) over the theme-muted grey
/// (`md_border_open`: SGR 37 on dark / SGR 90 on light). The markdown→cells
/// parser (`parse_markdown_to_cells`) supports SGR 2 and SGR 37/90 but SILENTLY
/// DROPS 256-colour (`38;5;N`) — a 256-colour grey would render at the DEFAULT
/// foreground (brighter, not dimmer) in the TUI. Faint over the muted grey is
/// the recessed look that actually survives that parser, on 16-colour and
/// modern terminals alike. Close: SGR 22 (clear faint) + SGR 39 (reset fg).
fn md_table_rule_wrap() -> (String, &'static str) {
    (
        format!("\x1b[2m{}", crate::highlight::theme::md_border_open()),
        "\x1b[22m\x1b[39m",
    )
}

/// Segmented horizontal rule for table separators — one segment per column
/// (`col_width + CELL_PADDING*2` characters wide), columns joined by
/// `COLUMN_GAP` spaces. This produces a visually segmented rule that mirrors
/// the column layout: `━━━━  ━━━━` rather than a solid full-width bar.
/// `preferred` selects the weight (`━` heavy for the header rule, `─` light
/// for inter-row separators). No vertical borders, corners, or junction glyphs.
fn render_table_rule(col_widths: &[usize], preferred: char, caps: TerminalCaps) -> String {
    let ch = stable_table_rule_char(preferred, caps);
    let gap = " ".repeat(TABLE_COLUMN_GAP);
    let segments: Vec<String> = col_widths
        .iter()
        .map(|&w| ch.to_string().repeat(w + TABLE_CELL_PADDING * 2))
        .collect();
    let rule = segments.join(&gap);
    if caps.colors {
        let (open, close) = md_table_rule_wrap();
        format!("{}{}{}", open, rule, close)
    } else {
        rule
    }
}

/// Pick the box-drawing rule glyph only when the terminal can actually render it,
/// else fall back to invariant-width ASCII. Two independent gates:
///
/// 1. `caps.unicode_symbols` — the SAME capability that governs every other
///    decorative glyph (`❯`, `◆`, box-drawing corners). It is on for modern
///    emulators (Windows Terminal via `WT_SESSION`, any `TERM_PROGRAM`, UTF-8
///    locales) and off for legacy Windows conhost, `TERM=dumb`, POSIX/`C`
///    locales, and `ATOMCODE_ASCII=1`. Gating tables on this (instead of a blunt
///    `cfg!(windows)`) lets a modern Windows Terminal show `━`/`─` like macOS,
///    while legacy conhost still drops to `=`/`-` so it never renders `□` tofu.
/// 2. Width safety — even on a Unicode-capable host, a glyph our width model
///    scores at ≠1 cell would desync column alignment, so force ASCII there too.
fn stable_table_rule_char(preferred: char, caps: TerminalCaps) -> char {
    let width_safe = crate::width::cell_char_width(preferred).unwrap_or(1) == 1;
    if caps.unicode_symbols && width_safe {
        preferred
    } else if preferred == '━' {
        '='
    } else {
        '-'
    }
}

/// Inter-row / header separator: a continuous full-width rule (see
/// [`render_table_rule`]). No vertical borders, corners, or junction glyphs —
/// content stays readable without recreating the ambiguous-width boxed layout.
fn render_table_separator(
    col_widths: &[usize],
    separator_char: char,
    caps: TerminalCaps,
) -> String {
    render_table_rule(col_widths, separator_char, caps)
}

/// Width-aware variant. When `max_width > 0` and the table can't fit at its
/// natural column widths, fall back to a flat key/value record format
/// (`header: cell` per line, blank line between rows) so no information is
/// lost to per-cell truncation. `max_width = 0` keeps aligned-table rendering
/// at natural widths regardless of size.
pub fn flush_aligned_table_with_width(
    rows: &[String],
    caps: TerminalCaps,
    max_width: usize,
) -> String {
    // Parse each row honouring code-span pipes + `\|` escape.
    let parsed: Vec<Vec<String>> = rows.iter().map(|r| split_table_row(r)).collect();

    // Identify separator row(s) — cells match `[-: ]+` only.
    let is_sep = is_separator_row;

    // A real GFM table REQUIRES a delimiter row (`---|---`). Since detection now
    // speculatively buffers any pipe-splitting line, a lone prose line with a
    // literal `|` (or a header with no following delimiter) lands here with NO
    // separator — render those as normal inline markdown, NOT a drawn box.
    let has_sep = parsed.iter().any(|r| is_sep(r));
    if !has_sep || parsed.len() < 2 {
        return rows
            .iter()
            .map(|r| render_inline_line(r, caps))
            .collect::<Vec<_>>()
            .join("\n");
    }

    let ncols = effective_ncols(&parsed);
    if ncols == 0 {
        return String::new();
    }

    // Compute natural column widths from non-separator rows. We do NOT cap
    // these — the cap-and-truncate-with-… approach the previous code took
    // chopped real content out of cells and made wide tables in narrow
    // terminals unreadable. Instead, if the natural table doesn't fit, the
    // flat-mode fallback below renders every cell in full.
    //
    let mut col_widths = vec![0usize; ncols];
    for row in &parsed {
        if is_sep(row) {
            continue;
        }
        for (j, cell) in row.iter().enumerate() {
            if j >= ncols {
                break;
            }
            let plain = strip_md_for_width(cell);
            let w = crate::width::display_width(&plain);
            col_widths[j] = col_widths[j].max(w);
        }
    }
    // Borderless Codex-style row width. Keep one column unused at the right
    // edge whenever a real viewport budget is available. Besides looking less
    // cramped, this prevents ConPTY from wrapping a line that exactly touches
    // the final cell while a resize or font fallback is being reconciled.
    let natural_row_width: usize = col_widths.iter().sum::<usize>()
        + TABLE_CELL_PADDING * 2 * ncols
        + TABLE_COLUMN_GAP * ncols.saturating_sub(1);
    let effective_max_width = if max_width > 0 {
        max_width.saturating_sub(TABLE_RIGHT_GUARD)
    } else {
        0
    };
    // `max_width == 0` alone means "unbounded". A real viewport of one column
    // also produces an effective budget of zero after reserving the guard, and
    // must still take the flat fallback rather than accidentally rendering an
    // unbounded table during a resize transition.
    if max_width > 0 && natural_row_width > effective_max_width {
        // Middle tier (codex-style graceful degradation): before collapsing to a
        // flat vertical list, try to keep the GRID by shrinking wide columns and
        // wrapping their cells. The absence of vertical border glyphs makes this
        // path independent of the host's East Asian Ambiguous-width policy.
        if let Some(shrunk) = fit_columns_to_grid(&parsed, &col_widths, ncols, effective_max_width)
        {
            // Starvation guard (codex-style): if keeping the grid squeezes a
            // column so hard that values fragment into tall strips, a flat
            // one-field-per-line record is more scannable.
            if !grid_too_starved(&parsed, &shrunk) {
                return render_wrapped_table(&parsed, &shrunk, caps);
            }
        }
        return render_flat_table(&parsed, caps);
    }

    let data_rows: Vec<&Vec<String>> = parsed.iter().filter(|r| !is_sep(r)).collect();

    let mut out = String::new();
    for (i, row) in data_rows.iter().enumerate() {
        for (j, w) in col_widths.iter().enumerate() {
            let cell = row.get(j).map(|s| s.as_str()).unwrap_or("");
            let plain_w = crate::width::display_width(&strip_md_for_width(cell));
            let mut body = render_inline(cell, caps);
            if i == 0 && caps.colors {
                body = format!(
                    "{}{}{}",
                    theme::md_heading_open(),
                    body,
                    theme::MD_HEADING_CLOSE
                );
            }
            out.push_str(&" ".repeat(TABLE_CELL_PADDING));
            out.push_str(&body);
            let pad = w.saturating_sub(plain_w);
            for _ in 0..pad {
                out.push(' ');
            }
            out.push_str(&" ".repeat(TABLE_CELL_PADDING));
            if j + 1 < col_widths.len() {
                out.push_str(&" ".repeat(TABLE_COLUMN_GAP));
            }
        }
        out.push('\n');
        if i + 1 < data_rows.len() {
            out.push_str(&render_table_separator(
                &col_widths,
                if i == 0 { '━' } else { '─' },
                caps,
            ));
            out.push('\n');
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Narrow-terminal fallback for tables that can't fit at natural column
/// widths. Each data row is expanded into N lines of `header：cell` (one
/// per column), with a blank line between successive rows. Soft-wrapping
/// of long lines is left to the caller's downstream wrap stage so the
/// terminal width budget is honoured without losing any cell content.
fn render_flat_table(parsed: &[Vec<String>], caps: TerminalCaps) -> String {
    let has_sep = parsed.iter().any(|r| is_separator_row(r));
    let mut data_iter = parsed.iter().filter(|r| !is_separator_row(r));

    // First non-sep row is treated as headers when a separator exists.
    // Without a separator the source isn't a real markdown table (it's
    // just `|` lines); fall back to printing every cell with no label.
    let headers: Vec<String> = if has_sep {
        match data_iter.next() {
            Some(h) => h.clone(),
            None => return String::new(),
        }
    } else {
        Vec::new()
    };

    let ncols = effective_ncols(parsed);
    // Right-pad every label to the widest header so the `：value` parts line up
    // into a scannable column (codex-style key/value record), instead of ragged
    // `Session：…` / `Detected：…`.
    let label_w = headers
        .iter()
        .map(|h| crate::width::display_width(&strip_md_for_width(h)))
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    let mut first = true;
    for row in data_iter {
        if !first {
            out.push('\n');
        }
        first = false;
        for j in 0..ncols {
            let cell = row.get(j).map(|s| s.as_str()).unwrap_or("");
            let cell_rendered = render_inline(cell, caps);
            if let Some(header) = headers.get(j) {
                let h_rendered = render_inline(header, caps);
                let hw = crate::width::display_width(&strip_md_for_width(header));
                out.push_str(&h_rendered);
                for _ in 0..label_w.saturating_sub(hw) {
                    out.push(' ');
                }
                out.push('：');
                out.push_str(&cell_rendered);
            } else {
                out.push_str(&cell_rendered);
            }
            out.push('\n');
        }
    }
    // Drop the trailing newline so the caller's `format!("{}\n{}", t, body)`
    // doesn't sprinkle an extra blank line after the block.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Strip markdown formatting markers (`**bold**`, `*italic*`, `` `code` ``)
/// to recover the *visible* string `render_inline` would produce, for
/// column-width measurement in table layout.
///
/// MUST mirror `render_inline`'s parsing exactly. Earlier this was a naive
/// `s.replace("**", "").replace('`', "")`, which broke alignment for cells
/// containing markdown-marker characters as **literal content inside an
/// inline-code span**: e.g. `` `src/**/*.ts` `` is rendered by
/// `render_inline` as styled "src/**/*.ts" (`**` is glob-pattern literal,
/// preserved inside the code span), but the old strip removed those `**`
/// too — `plain_w` came back 2 cells short and the right border of that
/// row was painted 2 cells past where the column was supposed to end.
/// Same misalignment in reverse for `*italic*`: render strips the single
/// `*` and emits "italic"; the old strip kept the `*` and overshot
/// `plain_w` by 2. Reported on the markdown tools-listing table where
/// the glob row was visibly nudged 2 cells right vs every other row.
///
/// Walk the same parser `render_inline` uses, emit only the inner text,
/// preserve unclosed markers verbatim (matching render's fallback).
fn strip_md_for_width(s: &str) -> String {
    // Keep width measurement in lockstep with `render_inline`: the display-only
    // circled-list spacing is visible content and therefore must participate in
    // table sizing and wrapping decisions as well.
    let s = normalize_circled_list_spacing(s);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    let mut inner = String::new();
                    let mut closed = false;
                    while let Some(&p) = chars.peek() {
                        if p == '*' {
                            chars.next();
                            if chars.peek() == Some(&'*') {
                                chars.next();
                                closed = true;
                                break;
                            } else {
                                inner.push('*');
                            }
                        } else {
                            chars.next();
                            inner.push(p);
                        }
                    }
                    if closed && !inner.is_empty() {
                        out.push_str(&inner);
                    } else {
                        out.push_str("**");
                        out.push_str(&inner);
                    }
                } else {
                    let mut inner = String::new();
                    let mut closed = false;
                    while let Some(&p) = chars.peek() {
                        chars.next();
                        if p == '*' {
                            closed = true;
                            break;
                        }
                        inner.push(p);
                    }
                    if closed && !inner.is_empty() {
                        out.push_str(&inner);
                    } else {
                        out.push('*');
                        out.push_str(&inner);
                    }
                }
            }
            '`' => {
                let mut inner = String::new();
                let mut closed = false;
                while let Some(&p) = chars.peek() {
                    chars.next();
                    if p == '`' {
                        closed = true;
                        break;
                    }
                    inner.push(p);
                }
                if closed && !inner.is_empty() {
                    out.push_str(&inner);
                } else {
                    out.push('`');
                    out.push_str(&inner);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Legacy single-line inline renderer — kept for direct callers (tests,
/// simple assistant lines). Does not track block state.
pub fn render_inline_line(line: &str, caps: TerminalCaps) -> String {
    render_inline(line, caps)
}

// ─── Helpers ───

fn render_inline(line: &str, caps: TerminalCaps) -> String {
    let line = normalize_circled_list_spacing(line);
    if !caps.colors {
        return line.into_owned();
    }
    let mut out = String::with_capacity(line.len() + 16);
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    let mut inner = String::new();
                    let mut closed = false;
                    while let Some(&p) = chars.peek() {
                        if p == '*' {
                            chars.next();
                            if chars.peek() == Some(&'*') {
                                chars.next();
                                closed = true;
                                break;
                            } else {
                                inner.push('*');
                            }
                        } else {
                            chars.next();
                            inner.push(p);
                        }
                    }
                    if closed && !inner.is_empty() {
                        out.push_str(theme::MD_BOLD_OPEN);
                        out.push_str(&inner);
                        out.push_str(theme::MD_BOLD_CLOSE);
                    } else {
                        out.push_str("**");
                        out.push_str(&inner);
                    }
                } else {
                    let mut inner = String::new();
                    let mut closed = false;
                    while let Some(&p) = chars.peek() {
                        chars.next();
                        if p == '*' {
                            closed = true;
                            break;
                        }
                        inner.push(p);
                    }
                    if closed && !inner.is_empty() {
                        out.push_str(theme::MD_ITALIC_OPEN);
                        out.push_str(&inner);
                        out.push_str(theme::MD_ITALIC_CLOSE);
                    } else {
                        out.push('*');
                        out.push_str(&inner);
                    }
                }
            }
            '`' => {
                let mut inner = String::new();
                let mut closed = false;
                while let Some(&p) = chars.peek() {
                    chars.next();
                    if p == '`' {
                        closed = true;
                        break;
                    }
                    inner.push(p);
                }
                if closed && !inner.is_empty() {
                    // Bright cyan, NOT bold (SGR 96 — see `md_inline_code_open`).
                    // Bold + bright cyan made every span flare in dense prose and
                    // rendered harshly on terminals that don't soften bright cyan;
                    // color alone marks it as code, weight is reserved for **bold**
                    // prose. It's a 16-colour SGR interpreted by the terminal's own
                    // palette, so it adapts to light/dark. The close `\x1b[22;39m`
                    // resets bold (a no-op now) + fg (SGR 39) so nothing bleeds.
                    out.push_str(theme::md_inline_code_open());
                    out.push_str(&inner);
                    out.push_str(theme::MD_INLINE_CODE_CLOSE);
                } else {
                    out.push('`');
                    out.push_str(&inner);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Add a visual separator after circled list numbers in ordinary prose.
///
/// Some Windows fonts let the right edge of `①`–`⑳` touch the following
/// glyph when a model emits compact text such as `：①Rust` or ` ②前端`.
/// Normalize only list-shaped boundaries and leave inline code and embedded
/// identifiers (`第①章`) byte-for-byte unchanged. This is shared by ephemeral
/// display projections; conversation/session source text is never rewritten.
pub(crate) fn normalize_circled_list_spacing(line: &str) -> Cow<'_, str> {
    normalize_circled_list_spacing_with_cursor(line, 0).0
}

fn visit_circled_list_separator_offsets(line: &str, mut visit: impl FnMut(usize)) {
    let mut chars = line.char_indices().peekable();
    let mut previous = None;
    let mut in_code = false;

    while let Some((index, ch)) = chars.next() {
        if ch == '`' {
            in_code = !in_code;
        } else if !in_code
            && ('\u{2460}'..='\u{2473}').contains(&ch)
            && previous.is_none_or(is_circled_list_boundary)
            && chars
                .peek()
                .is_some_and(|(_, next)| is_list_label_char(*next))
        {
            // The synthetic space bridges a 1-cell circled glyph to the next
            // label on fonts that paint these circles narrow (the original
            // Windows "：①Rust" touch). When the host paints the circled
            // number as a 2-cell fullwidth glyph (CJK fonts, iTerm2/etc.),
            // the glyph's own second cell already provides visual separation;
            // an extra space would double it to `①  Rust`. Match the
            // renderer's width model so spacing is added only when needed.
            if crate::width::cell_char_width(ch) == Some(1) {
                visit(index + ch.len_utf8());
            }
        }
        previous = Some(ch);
    }
}

/// Display-only circled-list spacing plus the corresponding cursor position.
///
/// The composer owns a byte cursor into the unmodified input buffer. Synthetic
/// separator bytes inserted before (or exactly at) that cursor must therefore
/// advance its display projection, while the source text and submitted payload
/// remain byte-for-byte unchanged.
pub(crate) fn normalize_circled_list_spacing_with_cursor(
    line: &str,
    cursor_byte: usize,
) -> (Cow<'_, str>, usize) {
    let mut output: Option<String> = None;
    let mut segment_start = 0;
    let source_cursor = cursor_byte.min(line.len());
    let mut display_cursor = source_cursor;

    visit_circled_list_separator_offsets(line, |end| {
        let output = output.get_or_insert_with(|| String::with_capacity(line.len() + 1));
        output.push_str(&line[segment_start..end]);
        output.push(' ');
        segment_start = end;
        if end <= source_cursor {
            display_cursor += 1;
        }
    });

    let display = match output {
        Some(mut output) => {
            output.push_str(&line[segment_start..]);
            Cow::Owned(output)
        }
        None => Cow::Borrowed(line),
    };
    (display, display_cursor)
}

/// Map a byte cursor in the display-only circled-list projection back to the
/// nearest grapheme boundary in the original composer buffer.
///
/// The retained renderer inserts a synthetic space after list-shaped circled
/// numbers (for example `①Rust` -> `① Rust`) without changing the submitted
/// text. Visual Up/Down navigation operates on that projected layout, so its
/// target cursor must be translated back before updating
/// [`Buffer`](crate::event_loop::Buffer). A target on either side of a
/// synthetic space maps to the same source boundary immediately after the
/// circled number.
pub(crate) fn source_cursor_from_circled_list_display_cursor(
    source: &str,
    display_cursor: usize,
) -> usize {
    let mut separator_offsets = Vec::new();
    visit_circled_list_separator_offsets(source, |offset| separator_offsets.push(offset));

    let mut best_source = 0usize;
    let mut best_distance = display_cursor;
    let mut separator_index = 0usize;

    for source_cursor in source
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .chain(std::iter::once(source.len()))
    {
        while separator_offsets
            .get(separator_index)
            .is_some_and(|offset| *offset <= source_cursor)
        {
            separator_index += 1;
        }
        let projected_cursor = source_cursor + separator_index;
        let distance = projected_cursor.abs_diff(display_cursor);
        if distance < best_distance || (distance == best_distance && source_cursor > best_source) {
            best_source = source_cursor;
            best_distance = distance;
        }
    }

    best_source
}

fn is_circled_list_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '：' | '，'
                | '。'
                | '；'
                | '！'
                | '？'
                | '、'
                | '（'
                | '）'
                | '【'
                | '】'
                | '《'
                | '》'
                | '“'
                | '”'
                | '‘'
                | '’'
        )
}

fn is_list_label_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
                | '\u{3040}'..='\u{30FF}'
                | '\u{AC00}'..='\u{D7AF}'
        )
}

/// Return the marker and marker width for a fenced-code opening line.
/// Requires at least 3 consecutive backticks or tildes.
///
/// Shared with `event_loop/commands::extract_code_blocks` so the render
/// pipeline and the `/copy` command agree on fence parsing (issue #699).
pub(crate) fn fence_start(line: &str) -> Option<(char, usize)> {
    let marker = line.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let len = line.chars().take_while(|&ch| ch == marker).count();
    (len >= 3).then_some((marker, len))
}

/// A closing fence must use the opening marker and be at least as long.
/// This keeps a ```` `````` (4-backtick) block from being closed by an
/// inner ```` ``` ```` (3-backtick) line, and prevents `~~~` from closing a
/// backtick block (issue #699).
///
/// Shared with `event_loop/commands::extract_code_blocks`.
pub(crate) fn is_closing_fence(line: &str, marker: char, opening_len: usize) -> bool {
    let len = line.chars().take_while(|&ch| ch == marker).count();
    len >= opening_len && line.chars().skip(len).all(char::is_whitespace)
}

fn is_hrule(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let first = trimmed.chars().next().unwrap();
    if first != '-' && first != '*' && first != '_' {
        return false;
    }
    let mut n = 0;
    for c in trimmed.chars() {
        if c == first {
            n += 1;
        } else if !c.is_whitespace() {
            return false;
        }
    }
    n >= 3
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let line = line.trim_start();
    let mut level = 0u8;
    for c in line.chars() {
        if c == '#' && level < 6 {
            level += 1;
        } else if level > 0 && c == ' ' {
            let content = &line[(level as usize) + 1..];
            return Some((level, content));
        } else {
            return None;
        }
    }
    None
}

/// Parsed list item: indent level, the marker string (e.g. "•", "1."),
/// and the remaining text after the marker.
struct ParsedListItem {
    indent: usize,
    marker: String,
    rest: String,
}

fn parse_list_item(line: &str) -> Option<ParsedListItem> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    let rest = &line[indent..];

    // Unordered: "- text" / "* text"
    if let Some(r) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
        return Some(ParsedListItem {
            indent,
            marker: "•".to_string(),
            rest: r.to_string(),
        });
    }

    // Ordered: "1. text" / "12. text" — one or more digits followed by ". "
    let digits_end = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_end > 0 {
        let after_digits = &rest[digits_end..];
        if let Some(r) = after_digits.strip_prefix(". ") {
            let marker = &rest[..digits_end]; // "1", "12", etc.
            return Some(ParsedListItem {
                indent,
                marker: format!("{}.", marker),
                rest: r.to_string(),
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::theme;
    use crate::terminal::{EnvView, TerminalCaps};

    fn caps() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
            lang: Some("en_US.UTF-8".to_string()),
            ..Default::default()
        })
    }
    fn plain_caps() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: true,
            term: Some("xterm".to_string()),
            lang: Some("en_US.UTF-8".to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn table_rule_colours_with_faint_not_dropped_256() {
        // `parse_markdown_to_cells` honours SGR 2 (faint) + SGR 37/90 but SILENTLY
        // DROPS 256-colour (`38;5;N`) — a 256-colour rule would render at full
        // brightness in the TUI. Guard: the coloured rule uses faint, never 256.
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: false,
            term: Some("xterm-256color".to_string()),
            lang: Some("en_US.UTF-8".to_string()),
            ..Default::default()
        });
        assert!(caps.colors, "test needs colours on");
        let rule = render_table_rule(&[3, 4], '─', caps);
        assert!(
            rule.contains("\x1b[2m"),
            "coloured rule must use faint (SGR 2): {rule:?}"
        );
        assert!(
            !rule.contains("38;5"),
            "must not use 256-colour (parser drops it): {rule:?}"
        );
    }

    /// The synthetic separator the circled-list spacing inserts between a
    /// circled number and its label, under THIS host's width model. Circled
    /// digits ①–⑳ are widened to 2 cells on emoji-capable terminals (macOS
    /// GUI terms, Windows Terminal), where the glyph's own second cell already
    /// separates it from the following label — so no extra space is added.
    /// On legacy narrow-font hosts (bare conhost) they stay 1 cell and the
    /// original spacing rule inserts a space. Tests must not hard-code one
    /// behavior or they break on the other platform.
    fn circled_sep() -> &'static str {
        if crate::width::cell_char_width('①') == Some(1) {
            " "
        } else {
            ""
        }
    }

    // Collect the rendered output of feeding `lines` one-by-one, then finalize.
    fn render_all(lines: &[&str]) -> String {
        let mut st = MdState::new();
        let mut out = String::new();
        for l in lines {
            if let Some(r) = render_line(l, &mut st, plain_caps()) {
                out.push_str(&r);
                out.push('\n');
            }
        }
        if let Some(r) = finalize(&mut st, plain_caps()) {
            out.push_str(&r);
        }
        out
    }

    fn count(hay: &str, needle: &str) -> usize {
        hay.matches(needle).count()
    }

    fn has_table_rule(rendered: &str, rule: char) -> bool {
        rendered.lines().any(|line| {
            let line = line.trim();
            line.contains(rule)
                && line
                    .chars()
                    .all(|ch| ch == rule || ch.is_ascii_whitespace())
        })
    }

    #[test]
    fn duplicate_heading_after_table_is_dropped() {
        // Weak-model artefact: same heading emitted twice with only a (stray)
        // table between. The repeat must be dropped; the table content kept.
        let out = render_all(&[
            "| a | b |",
            "|---|---|",
            "| 1 | 2 |",
            "## 三、未使用依赖分析",
            "| c | d |",
            "|---|---|",
            "| 3 | 4 |",
            "## 三、未使用依赖分析",
        ]);
        assert_eq!(
            count(&out, "三、未使用依赖分析"),
            1,
            "consecutive identical heading (table between) must render once:\n{out}"
        );
    }

    #[test]
    fn duplicate_heading_across_blank_line_is_dropped() {
        let out = render_all(&["## Section", "", "## Section"]);
        assert_eq!(
            count(&out, "Section"),
            1,
            "blank line must not un-dedup:\n{out}"
        );
    }

    #[test]
    fn heading_reused_after_real_content_is_kept() {
        // Legitimate: same heading in two sections with prose between — NOT a
        // duplicate, both must render.
        let out = render_all(&["## Example", "some explanatory prose here", "## Example"]);
        assert_eq!(
            count(&out, "Example"),
            2,
            "content between must keep both:\n{out}"
        );
    }

    #[test]
    fn heading_reused_after_list_item_is_kept() {
        let out = render_all(&["## Notes", "- a bullet point", "## Notes"]);
        assert_eq!(
            count(&out, "Notes"),
            2,
            "list content must keep both:\n{out}"
        );
    }

    #[test]
    fn different_consecutive_headings_both_render() {
        let out = render_all(&["## First", "## Second"]);
        assert_eq!(count(&out, "First"), 1);
        assert_eq!(count(&out, "Second"), 1);
    }

    #[test]
    fn split_table_row_handles_pipe_in_inline_code() {
        let row = "| 闭包 | `|a, b| b.cmp(&a)` |";
        assert_eq!(
            split_table_row(row),
            vec!["闭包".to_string(), "`|a, b| b.cmp(&a)`".to_string()],
        );
    }

    #[test]
    fn split_table_row_handles_multiple_code_spans_with_pipes() {
        let row = "| `cat | grep` | `int | str` | result |";
        assert_eq!(
            split_table_row(row),
            vec![
                "`cat | grep`".to_string(),
                "`int | str`".to_string(),
                "result".to_string(),
            ],
        );
    }

    #[test]
    fn split_table_row_honours_backslash_escape() {
        let row = "| literal \\| pipe | next |";
        assert_eq!(
            split_table_row(row),
            vec!["literal | pipe".to_string(), "next".to_string()],
        );
    }

    #[test]
    fn split_table_row_plain_no_codespan_unchanged() {
        let row = "| a | b | c |";
        assert_eq!(
            split_table_row(row),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );
    }

    #[test]
    fn split_table_row_no_leading_or_trailing_delim() {
        let row = "a | b | c";
        assert_eq!(
            split_table_row(row),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );
    }

    #[test]
    fn split_table_row_empty_cell_preserved() {
        let row = "| a |  | c |";
        assert_eq!(
            split_table_row(row),
            vec!["a".to_string(), "".to_string(), "c".to_string()],
        );
    }

    #[test]
    fn split_table_row_with_cjk_content() {
        let row = "| 模式匹配 | `Foo | Bar` 多模式 |";
        assert_eq!(
            split_table_row(row),
            vec!["模式匹配".to_string(), "`Foo | Bar` 多模式".to_string(),],
        );
    }

    #[test]
    fn flush_aligned_table_rust_closure_renders_two_columns() {
        // End-to-end: a table cell containing a Rust closure should
        // NOT explode into phantom columns. Before the split_table_row
        // fix, `|a, b|` inside the cell got split as separators and
        // the body row showed phantom columns. The borderless renderer must
        // preserve the literal closure pipes as content without introducing
        // any structural box glyphs.
        let rows = vec![
            "| 元素 | 示例 |".to_string(),
            "| --- | --- |".to_string(),
            "| 闭包 | `|a, b| b.cmp(&a)` |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        let body_line = out
            .lines()
            .find(|l| l.contains("闭包"))
            .expect("body row missing");
        assert!(body_line.contains("|a, b| b.cmp(&a)"));
        assert!(!body_line.contains('│'), "borderless row: {body_line:?}");
        assert!(!out.contains("---"), "raw delimiter leaked: {out}");
    }

    #[test]
    fn flush_aligned_table_with_emoji_symbol_keeps_rows_aligned() {
        // ☀ (U+2600) is a legacy-block pictographic emoji that GUI terminals
        // paint at 2 cells. The renderer must SIZE and PAD the column with the
        // same width metric (crate::width::display_width) so every row's
        // border lands in the same column. Regression for the emoji-symbol
        // table-misalignment bug (stray `│` shifted right of "☀ 晴").
        let rows = vec![
            "| 时段 | 天气 |".to_string(),
            "| --- | --- |".to_string(),
            "| 早上 | ☀ 晴 |".to_string(),
            "| 中午 | 多云 |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        let widths: Vec<usize> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(crate::width::display_width)
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "every table row must share one display width; got {:?} for\n{}",
            widths,
            out
        );
    }

    #[test]
    fn table_rules_are_segmented_per_column() {
        // Separator lines are per-column segments (col_width + padding*2 rule chars
        // each), joined by COLUMN_GAP spaces — not a solid full-width bar.
        // No top/bottom boundary rows are emitted; the table starts with the
        // header row and ends with the last data row.
        let rows = vec![
            "| 命令 | 行为 | 前台会话 |".to_string(),
            "| --- | --- | --- |".to_string(),
            "| /fork | 分叉新会话 | 原会话保留 |".to_string(),
            "| /bg | 挪到后台 | 全新空会话 |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);

        let is_rule = |t: &str| {
            !t.is_empty()
                && t.chars().all(|c| matches!(c, '━' | '─' | '=' | '-' | ' '))
                && t.chars().any(|c| matches!(c, '━' | '─' | '=' | '-'))
        };
        let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
        // No top/bottom boundary: the table starts with the header ROW and ends
        // with the last DATA ROW — neither the first nor last line is a rule.
        assert!(
            !is_rule(lines.first().unwrap().trim()),
            "no top boundary row"
        );
        assert!(
            !is_rule(lines.last().unwrap().trim()),
            "no bottom boundary row"
        );
        // Every rule line is SEGMENTED (3 columns → inter-segment gaps), not a
        // solid full-width bar (a regression to the old solid rule has NO spaces).
        for l in &lines {
            let t = l.trim();
            if is_rule(t) {
                assert!(
                    t.contains(' '),
                    "rule must be segmented (gaps present): {t:?}"
                );
            }
        }
        let rule_lines: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| is_rule(l.trim()))
            .collect();

        // Borderless design: header separator + 1 inter-row separator only (no top/bottom).
        assert!(
            rule_lines.len() >= 2,
            "expected at least header + inter-row separators, got {}:\n{out}",
            rule_lines.len()
        );

        // Each separator must be segmented: it must contain an interior space
        // (the COLUMN_GAP between adjacent column segments) — not a solid bar.
        for line in &rule_lines {
            assert!(
                line.contains(' '),
                "rule line must be segmented (interior gap between column segments): {:?}\n{out}",
                line
            );
        }

        // No vertical border chars.
        assert!(!out.contains('│') && !out.contains('┼'));
    }

    #[test]
    fn table_rule_char_follows_unicode_symbols_capability() {
        // A Unicode-capable terminal (modern engine + UTF-8 locale) keeps the
        // box-drawing rules — the same `unicode_symbols` gate that governs ❯/◆.
        let uni = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            term: Some("xterm-256color".to_string()),
            lang: Some("en_US.UTF-8".to_string()),
            ..Default::default()
        });
        assert!(uni.unicode_symbols, "UTF-8 xterm must be unicode-capable");
        // Width model must also agree the glyph is one cell (true on the test host).
        if crate::width::cell_char_width('━') == Some(1) {
            assert_eq!(stable_table_rule_char('━', uni), '━');
            assert_eq!(stable_table_rule_char('─', uni), '─');
        }
        // A host that can't render Unicode symbols (legacy conhost / POSIX locale /
        // ATOMCODE_ASCII / dumb) must drop to invariant-width ASCII — no □ tofu.
        let ascii = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            term: Some("xterm".to_string()),
            lang: Some("C".to_string()),
            ..Default::default()
        });
        assert!(!ascii.unicode_symbols, "C locale must force ASCII fallback");
        assert_eq!(stable_table_rule_char('━', ascii), '=');
        assert_eq!(stable_table_rule_char('─', ascii), '-');
    }

    /// Regression for issue #708: Markdown tables with CJK characters must
    /// have aligned borders regardless of whether East Asian Ambiguous
    /// box-drawing glyphs (`│` `─` `┌` etc.) render at width 1 or 2.
    /// The fix rounds column widths so `(w + 2)` is a multiple of the
    /// dash width, preventing border rows from overshooting data rows.
    ///
    /// This test uses the exact example from the issue: a 6-column
    /// weather table with CJK content.
    #[test]
    fn cjk_table_borders_align_with_mixed_width_cells() {
        let rows = vec![
            "| 日期 | 天气 | 温度 | 降水总量 | 降水概率 | 最大风速 |".to_string(),
            "|------|------|------|----------|----------|----------|".to_string(),
            "| 6/22（今天） | 强雷暴伴冰雹 | 23.3 ~ 31°C | 33.2 mm | 98% | 17 km/h |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 120);
        let widths: Vec<usize> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(crate::width::display_width)
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "every table row must share one display width (issue #708); got {:?} for\n{}",
            widths,
            out
        );
    }

    /// Regression for issue #708 extended: weather table with emoji + CJK
    /// content. The user reported that rows containing emoji like 🌤 and 🌡
    /// had their right border `│` shifted right compared to rows without
    /// emoji. The root cause is that `display_width` (used for column-width
    /// and padding computation) and the actual rendered width of the cell
    /// diverge when the cell content contains emoji that are wider than
    /// `unicode-width` reports, or when `strip_md_for_width` and
    /// `render_inline` disagree on the visible width.
    #[test]
    fn emoji_cjk_weather_table_borders_aligned() {
        let rows = vec![
            "| 项目 | 信息 |".to_string(),
            "|------|------|".to_string(),
            "| 📅 日期 | 2026年06月22日 |".to_string(),
            "| 🌤 天气状况 | 大雨 💧 |".to_string(),
            "| 🌡 气温范围 | 24℃ ~ 30℃ |".to_string(),
            "| 🍃 风向风力 | 西北风 3级 |".to_string(),
            "| 💧 相对湿度 | 96% |".to_string(),
            "| ☀  紫外线 | 最弱 |".to_string(),
            "| 🍃 空气质量 | 优 (AQI 28) |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        let widths: Vec<usize> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(crate::width::display_width)
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "every row in emoji+CJK weather table must share one display width; got {:?} for\n{}",
            widths,
            out
        );
    }

    /// Companion: a simpler 2-column CJK table that also verifies
    /// border alignment. This is the minimal repro — even a table
    /// with only CJK header cells must have consistent row widths.
    #[test]
    fn cjk_table_simple_two_column_aligned() {
        let rows = vec![
            "| 项目 | 状态 |".to_string(),
            "|------|------|".to_string(),
            "| 开发 | 进行中 |".to_string(),
            "| 测试 | 已完成 |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        let widths: Vec<usize> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(crate::width::display_width)
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "simple CJK table rows must be aligned; got {:?} for\n{}",
            widths,
            out
        );
    }

    #[test]
    fn inline_bold() {
        assert_eq!(
            render_inline_line("**bold**", caps()),
            format!("{}bold{}", theme::MD_BOLD_OPEN, theme::MD_BOLD_CLOSE)
        );
    }

    #[test]
    fn inline_italic() {
        assert_eq!(
            render_inline_line("*em*", caps()),
            format!("{}em{}", theme::MD_ITALIC_OPEN, theme::MD_ITALIC_CLOSE)
        );
    }

    #[test]
    fn inline_code() {
        // Inline code is colour-only (NOT bold) so it doesn't flare in dense
        // prose; weight is reserved for **bold**. The close sequence resets fg.
        let rendered = render_inline_line("`x`", caps());
        assert!(
            rendered.contains(theme::md_inline_code_open()),
            "inline code must open with MD_INLINE_CODE_OPEN: {}",
            rendered
        );
        assert!(
            rendered.contains(theme::MD_INLINE_CODE_CLOSE),
            "inline code must close with MD_INLINE_CODE_CLOSE: {}",
            rendered
        );
        // Must NOT be bold — the flare fix. (Bold bright cyan was `\x1b[1;96m`.)
        assert!(
            !rendered.contains("\x1b[1;96m") && !rendered.contains("\x1b[1;35m"),
            "inline code must NOT be bold: {}",
            rendered
        );
        assert!(
            !rendered.contains("\x1b[1;97m"),
            "inline code must NOT include bright-white SGR 97: {}",
            rendered
        );
        assert!(
            !rendered.contains("\x1b[1;38;2;"),
            "inline code must NOT include truecolor RGB: {}",
            rendered
        );
    }

    #[test]
    fn circled_list_numbers_gain_visual_spacing_at_prose_boundaries() {
        // The spacing rule is width-aware: a 1-cell circled glyph (legacy
        // narrow-font hosts) gets a synthetic space, a 2-cell glyph (emoji-
        // capable terminals) already separates itself. `circled_sep()`
        // resolves which applies on the host running the tests.
        let sep = circled_sep();
        assert_eq!(
            render_inline_line("现在批量落地：①Rust 修复 ②前端修复", plain_caps()),
            format!("现在批量落地：①{sep}Rust 修复 ②{sep}前端修复")
        );
        assert_eq!(
            render_inline_line("③Task", plain_caps()),
            format!("③{sep}Task")
        );
        assert_eq!(
            render_inline_line("⑳版本", plain_caps()),
            format!("⑳{sep}版本")
        );
    }

    #[test]
    fn circled_list_spacing_preserves_code_existing_spaces_and_identifiers() {
        assert_eq!(
            render_inline_line("`①Rust` 和 `②前端`", plain_caps()),
            "`①Rust` 和 `②前端`"
        );
        assert_eq!(
            render_inline_line("① Rust、② 前端、第①章、build①Rust", plain_caps()),
            "① Rust、② 前端、第①章、build①Rust"
        );
    }

    #[test]
    fn circled_list_spacing_maps_composer_cursor_without_mutating_source() {
        let source = "①Rust、②前端";
        let sep = circled_sep();

        let (display, cursor_after_first_label) =
            normalize_circled_list_spacing_with_cursor(source, '①'.len_utf8());
        assert_eq!(display, format!("①{sep}Rust、②{sep}前端"));
        assert_eq!(cursor_after_first_label, '①'.len_utf8() + sep.len());

        let (display, cursor_at_end) =
            normalize_circled_list_spacing_with_cursor(source, source.len());
        assert_eq!(display, format!("①{sep}Rust、②{sep}前端"));
        assert_eq!(cursor_at_end, source.len() + 2 * sep.len());
        assert_eq!(source, "①Rust、②前端");
    }

    #[test]
    fn circled_list_cursor_mapping_ignores_code_and_existing_spaces() {
        let source = "`①Rust` ② 前端";
        let (display, cursor) = normalize_circled_list_spacing_with_cursor(source, source.len());
        assert_eq!(display, source);
        assert_eq!(cursor, source.len());
    }

    #[test]
    fn circled_list_display_cursor_maps_back_to_source_boundary() {
        let source = "①Rust";
        let after_label = '①'.len_utf8();
        let sep = circled_sep();

        // With a synthetic separator (width-1 hosts) both sides of it map to
        // the same source insertion point, and later display positions account
        // for the extra byte. Without one (width-2 hosts) the display
        // projection is byte-identical to the source, so cursors map 1:1.
        assert_eq!(
            source_cursor_from_circled_list_display_cursor(source, after_label),
            after_label
        );
        assert_eq!(
            source_cursor_from_circled_list_display_cursor(source, after_label + sep.len()),
            after_label
        );
        assert_eq!(
            source_cursor_from_circled_list_display_cursor(source, source.len() + sep.len()),
            source.len()
        );
    }

    #[test]
    fn circled_list_spacing_keeps_inline_code_styled_content_unchanged() {
        let sep = circled_sep();
        let rendered = render_inline_line("说明：①Rust，代码 `②前端`", caps());
        assert!(rendered.contains(&format!("说明：①{sep}Rust，代码 ")));
        assert!(rendered.contains(&format!(
            "{}②前端{}",
            theme::md_inline_code_open(),
            theme::MD_INLINE_CODE_CLOSE
        )));
    }

    #[test]
    fn circled_list_spacing_is_included_in_table_width_measurement() {
        let sep = circled_sep();
        assert_eq!(
            strip_md_for_width("说明：①Rust ②前端"),
            format!("说明：①{sep}Rust ②{sep}前端")
        );
        assert_eq!(strip_md_for_width("`①Rust` 第①章"), "①Rust 第①章");
    }

    #[test]
    fn circled_list_spacing_keeps_table_borders_aligned() {
        let sep = circled_sep();
        let rows = vec![
            "| 项目 | 状态 |".to_string(),
            "| --- | --- |".to_string(),
            "| ①Rust | done |".to_string(),
            "| ②前端 | pending |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        let widths: Vec<usize> = out
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(crate::width::display_width)
            .collect();

        assert!(
            out.contains(&format!("①{sep}Rust")),
            "first label was not normalized: {out}"
        );
        assert!(
            out.contains(&format!("②{sep}前端")),
            "second label was not normalized: {out}"
        );
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "every table row must share one display width; got {widths:?} for\n{out}"
        );
    }

    #[test]
    fn circled_list_spacing_survives_narrow_table_fallback() {
        let sep = circled_sep();
        let rows = vec![
            "| 项目 | 很长的状态说明 |".to_string(),
            "| --- | --- |".to_string(),
            "| ①Rust | 正在执行一个很长的任务 |".to_string(),
            "| ②前端 | 等待处理另一个很长的任务 |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 20);

        assert!(
            out.contains(&format!("①{sep}Rust")),
            "first label was not normalized: {out}"
        );
        assert!(
            out.contains(&format!("②{sep}前端")),
            "second label was not normalized: {out}"
        );
    }

    #[test]
    fn fenced_code_block_colors_off_renders_plain_indented() {
        // With NO_COLOR / non-TTY caps, code blocks remain plain 2-space-indented
        // text with NO ANSI bytes. Pins the no-color invariant.
        let mut state = MdState::new();
        let _ = render_line("```", &mut state, plain_caps()); // open fence, no lang
        assert!(render_line("let x = 1;", &mut state, plain_caps()).is_none());
        let out = render_line("```", &mut state, plain_caps()).unwrap();
        assert!(
            out.contains("  let x = 1;"),
            "code body must appear with 2-space indent: {:?}",
            out
        );
        assert!(
            !out.contains('\x1b'),
            "colors-off must emit zero ANSI bytes: {:?}",
            out
        );
        assert!(!out.contains('│'), "no `│` gutter glyph: {:?}", out);
    }

    #[test]
    fn fenced_code_block_known_lang_emits_plain_indent_no_ansi() {
        // Plan-0 contract (see `highlight_block`'s doc): even with colors
        // enabled and a known language tag, the close-fence flush emits
        // plain 2-space-indented source with zero ANSI. Per-token
        // colouring was dropped because macOS Terminal.app's selection
        // overlay made truecolor tokens unreadable inside selections.
        let mut state = MdState::new();
        let _ = render_line("```rust", &mut state, caps());
        assert!(render_line("fn main() {}", &mut state, caps()).is_none());
        let out = render_line("```", &mut state, caps()).unwrap();
        assert!(
            out.contains("  fn main() {}"),
            "indent + body preserved: {:?}",
            out
        );
        assert!(
            !out.contains('\x1b'),
            "expected zero ANSI bytes under plan-0, got: {:?}",
            out
        );
    }

    #[test]
    fn fenced_cjk_diagram_repairs_display_but_preserves_copy_source() {
        let mut state = MdState::new();
        let source_lines = [
            "┌────────────┐",
            "│ config     │",
            "│ 配置         │",
            "│ auth       │",
            "└────────────┘",
        ];
        let source = source_lines.join("\n");

        assert!(render_line("```text", &mut state, plain_caps()).is_none());
        for line in source_lines {
            assert!(render_line(line, &mut state, plain_caps()).is_none());
        }
        let rendered = render_line("```", &mut state, plain_caps()).unwrap();

        let widths = rendered
            .lines()
            .map(crate::width::display_width)
            .collect::<Vec<_>>();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "diagram rows must align after markdown rendering: {widths:?}\n{rendered}"
        );
        assert_eq!(
            state.last_code_block_source.as_deref(),
            Some(source.as_str()),
            "display normalization must not alter /copy or persisted source"
        );
    }

    #[test]
    fn fenced_code_block_unknown_lang_emits_plain_indent() {
        // Plan-0: regardless of lang tag (known, unknown, or absent),
        // the body is emitted verbatim with a 2-space indent and zero
        // ANSI. This test pins the unknown-lang case specifically;
        // the known-lang case is covered by
        // `fenced_code_block_known_lang_emits_plain_indent_no_ansi`.
        let mut state = MdState::new();
        let _ = render_line("```frobnicate", &mut state, caps());
        assert!(render_line(r#"x = "hello""#, &mut state, caps()).is_none());
        let out = render_line("```", &mut state, caps()).unwrap();
        assert!(
            out.contains(r#"x = "hello""#),
            "unknown-lang body must survive verbatim: {:?}",
            out
        );
        assert!(
            !out.contains("\x1b["),
            "unknown lang must emit zero ANSI: {:?}",
            out
        );
    }

    #[test]
    fn plain_pass_through() {
        assert_eq!(render_inline_line("**b**", plain_caps()), "**b**");
    }

    #[test]
    fn heading_styled() {
        let mut st = MdState::new();
        let out = render_line("## Hello", &mut st, caps()).unwrap();
        assert!(out.contains("Hello"));
        // H1-H3 use the heading colour so they sit on a separate
        // colour layer from default-colour body text.
        assert!(
            out.contains(theme::md_heading_open()),
            "H2 should use MD_HEADING_OPEN, got: {:?}",
            out
        );
    }

    #[test]
    fn heading_h4_uses_italic_not_color() {
        let mut st = MdState::new();
        let out = render_line("#### Sub-deep", &mut st, caps()).unwrap();
        assert!(out.contains("Sub-deep"));
        // H4+ keeps italic-only — distinct from coloured H1-H3 without
        // adding a third colour tier.
        assert!(
            out.contains(theme::MD_ITALIC_OPEN),
            "H4 should use MD_ITALIC_OPEN, got: {:?}",
            out
        );
        assert!(
            !out.contains(theme::md_heading_open()),
            "H4 must not pick up the H1-H3 heading colour"
        );
    }

    #[test]
    fn heading_plain_keeps_hashes() {
        let mut st = MdState::new();
        let out = render_line("### Sub", &mut st, plain_caps()).unwrap();
        assert_eq!(out, "### Sub");
    }

    #[test]
    fn fence_toggles_state_open_close_with_buffering() {
        // Updated for buffer-and-flush:
        //   - open fence sets in_code_block, returns None (no body yet)
        //   - body lines return None and accumulate to code_buf
        //   - inline markdown inside a buffered line is preserved verbatim
        //     (we flush as code, not as inline markdown)
        //   - close fence flushes everything, resets state, returns Some(...)
        let mut st = MdState::new();
        assert!(render_line("```rust", &mut st, plain_caps()).is_none());
        assert!(st.in_code_block);

        // Body lines are buffered, not emitted.
        assert!(render_line("let x = 1;", &mut st, plain_caps()).is_none());
        assert!(render_line("**not bold**", &mut st, plain_caps()).is_none());
        assert_eq!(st.code_buf.len(), 2);

        // Close fence flushes — final output contains both body lines,
        // and the **not bold** markdown is preserved literally (not interpreted).
        // Using plain_caps so substring assertions aren't broken by ANSI interleave.
        let out = render_line("```", &mut st, plain_caps()).unwrap();
        assert!(out.contains("let x = 1;"));
        assert!(
            out.contains("**not bold**"),
            "inline markdown inside code must be preserved literally: {:?}",
            out
        );
        assert!(!st.in_code_block);
        assert!(st.code_buf.is_empty());
    }

    #[test]
    fn plain_markdown_never_sets_code_block_source_or_count() {
        // Invariant behind auto-copy: ordinary prose — numbered bold headers +
        // indented list items with INLINE `code` — must never set
        // `last_code_block_source` nor bump `code_block_count`. Only a real ```
        // fence can. So a "代码块已复制" hint can NEVER attach to non-fenced
        // content (rules out the "auto-copy fired on plain prose" reading).
        let md = "\
✅ **要写的内容**

1. **技术栈（一句话）**
   这是一个 Vue 3 + TypeScript 项目，使用 `Pinia` 做状态管理、`Tailwind` 做样式、`Vitest` 做单元测试。

2. **项目特有的约定（命令式，清晰直接）**
   - 组件一律使用 `<script setup lang=\"ts\">` Composition API
   - 样式只写 `Tailwind`，不写内联 style 和单独的 .css 文件
   - 所有 API 调用走 `src/services/*`，不在组件里直接 fetch

3. **常用命令（让 AI 知道怎么跑测试/构建）**
   - 启动开发：`pnpm dev`
   - 类型检查：`pnpm typecheck`
   - 测试：`pnpm test`

4. **禁区（哪些文件不能动）**
   - 不要修改 `src/generated/*`，那是从 OpenAPI 自动生成的

× **不要写的内容**";
        let mut st = MdState::new();
        let mut sets: Vec<String> = Vec::new();
        for line in md.split('\n') {
            let _ = render_line(line, &mut st, caps());
            if let Some(s) = &st.last_code_block_source {
                sets.push(format!("after {:?} → source={:?}", line, s));
            }
        }
        assert!(
            sets.is_empty(),
            "plain markdown set last_code_block_source (should be impossible without a ``` fence): {:#?}",
            sets
        );
        assert_eq!(st.code_block_count, 0, "no fenced block ⇒ count stays 0");
        assert!(!st.in_code_block, "no fence opened ⇒ never in a code block");
    }

    #[test]
    fn hrule_becomes_blank_line() {
        // Horizontal rules now render as blank lines (thematic break), not
        // visible rules — a line of "─" chars is visually noisier than the
        // blank separator it's supposed to stand in for.
        let mut st = MdState::new();
        let out = render_line("---", &mut st, caps()).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn list_bullets() {
        // Pin the dark palette so the marker's final ANSI is deterministic
        // (SGR 37). Asserting the literal escape sequence — not
        // `theme::md_marker_open()` — keeps the regression guard: if the
        // production helper regresses to SGR 90 on dark, this fails.
        let _guard = theme::test_lock();
        theme::set_theme_mode(false);
        let mut st = MdState::new();
        let out = render_line("- item", &mut st, caps()).unwrap();
        assert!(
            out.contains(&format!("{}•{}", "\x1b[37m", theme::MD_MUTED_CLOSE)),
            "bullet must use the dark marker colour (SGR 37): {:?}",
            out
        );
        assert!(out.contains("item"));
    }

    #[test]
    fn list_bullets_plain_caps_no_ansi() {
        let mut st = MdState::new();
        let out = render_line("- item", &mut st, plain_caps()).unwrap();
        // No colour → plain "• item" without any SGR.
        assert_eq!(out, "• item");
    }

    #[test]
    fn list_nested_indent() {
        let _guard = theme::test_lock();
        theme::set_theme_mode(false);
        let mut st = MdState::new();
        let out = render_line("  - nested", &mut st, caps()).unwrap();
        assert!(
            out.starts_with(&format!("  {}•{}", "\x1b[37m", theme::MD_MUTED_CLOSE)),
            "nested bullet with indent: {:?}",
            out
        );
    }

    #[test]
    fn ordered_list_single_digit() {
        let _guard = theme::test_lock();
        theme::set_theme_mode(false);
        let mut st = MdState::new();
        let out = render_line("1. first item", &mut st, caps()).unwrap();
        assert!(
            out.contains(&format!("{}1.{}", "\x1b[37m", theme::MD_MUTED_CLOSE)),
            "ordered marker must use the dark marker colour (SGR 37): {:?}",
            out
        );
        assert!(out.contains("first item"));
    }

    #[test]
    fn ordered_list_double_digit() {
        let _guard = theme::test_lock();
        theme::set_theme_mode(false);
        let mut st = MdState::new();
        let out = render_line("12. twelfth item", &mut st, caps()).unwrap();
        assert!(
            out.contains(&format!("{}12.{}", "\x1b[37m", theme::MD_MUTED_CLOSE)),
            "double-digit marker must use the dark marker colour (SGR 37): {:?}",
            out
        );
        assert!(out.contains("twelfth item"));
    }

    #[test]
    fn ordered_list_plain_caps_no_ansi() {
        let mut st = MdState::new();
        let out = render_line("3. third", &mut st, plain_caps()).unwrap();
        // No colour → plain "3. third" without any SGR.
        assert_eq!(out, "3. third");
    }

    #[test]
    fn ordered_list_nested() {
        // P1 regression: this test was still asserting the OLD fixed
        // MD_MUTED_OPEN (SGR 90), which can never match the production
        // theme-aware marker output (SGR 37 on dark) and fails under the
        // default MODE_DARK. Pin the dark palette and assert the literal
        // escape sequence like the sibling list tests.
        let _guard = theme::test_lock();
        theme::set_theme_mode(false);
        let mut st = MdState::new();
        let out = render_line("  5. nested ordered", &mut st, caps()).unwrap();
        assert!(
            out.starts_with(&format!("  {}5.{}", "\x1b[37m", theme::MD_MUTED_CLOSE)),
            "nested ordered with indent: {:?}",
            out
        );
        assert!(out.contains("nested ordered"));
    }

    #[test]
    fn number_dot_without_space_is_not_list() {
        // "3.text" (no space after dot) should NOT be parsed as a list item.
        let mut st = MdState::new();
        let out = render_line("3.text", &mut st, caps()).unwrap();
        assert!(
            !out.contains(theme::MD_MUTED_OPEN),
            "no muted marker: {:?}",
            out
        );
        assert!(out.contains("3.text"));
    }

    #[test]
    fn cjk_bold() {
        assert_eq!(
            render_inline_line("**你好**", caps()),
            format!("{}你好{}", theme::MD_BOLD_OPEN, theme::MD_BOLD_CLOSE)
        );
    }

    /// Wide-enough terminal: render as a borderless aligned-column table at the
    /// table's natural column widths. No top/bottom boundary, no truncation, no ellipsis.
    #[test]
    fn wide_table_renders_borderless_at_natural_widths() {
        let rows = vec![
            "| Feature | Status |".to_string(),
            "|---------|--------|".to_string(),
            "| login   | done   |".to_string(),
            "| signup  | wip    |".to_string(),
        ];
        // Plenty of room — natural width is well under 80.
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        assert!(!out.contains("---"), "raw delimiter leaked: {out}");
        assert!(!out.contains('┌') && !out.contains('│') && !out.contains('└'));
        // Cell contents survive in full.
        assert!(out.contains("login"));
        assert!(out.contains("signup"));
        // No ellipsis introduced.
        assert!(!out.contains('…'));
        // New design: header row + header-separator + data row + inter-row-separator + data row = 5 lines.
        // (No top/bottom boundary rows.)
        assert_eq!(
            out.lines().count(),
            5,
            "header, header-separator, two data rows and one inter-row separator:\n{out}"
        );
        let header_rule = stable_table_rule_char('━', plain_caps());
        let body_rule = stable_table_rule_char('─', plain_caps());
        // Exactly one heavy header separator line (no top/bottom boundaries).
        assert_eq!(
            out.lines()
                .filter(|line| {
                    let t = line.trim();
                    !t.is_empty()
                        && t.chars().all(|ch| ch == header_rule || ch == ' ')
                        && t.chars().any(|ch| ch == header_rule)
                })
                .count(),
            1,
            "exactly one header separator expected (no top/bottom boundaries):\n{out}"
        );
        // Inter-row (light) separator must be present.
        assert!(
            has_table_rule(&out, body_rule),
            "inter-row body rule missing:\n{out}"
        );
        // Separators are segmented — they contain interior spaces (column gaps).
        assert!(
            out.lines().any(|line| {
                let t = line.trim();
                !t.is_empty()
                    && t.contains(' ')
                    && t.chars()
                        .all(|ch| ch == header_rule || ch == body_rule || ch == ' ')
            }),
            "separator lines must be segmented (interior gap between column segments):\n{out}"
        );
        let styled = flush_aligned_table_with_width(&rows, caps(), 80);
        assert!(
            styled.contains(theme::md_heading_open()) && styled.contains(theme::MD_HEADING_CLOSE),
            "styled terminals should emphasize the table header:\n{styled}"
        );
    }

    #[test]
    fn pipeless_gfm_table_is_detected_and_rendered() {
        // GFM tables WITHOUT leading/trailing pipes (`a | b` + `---|---`) must
        // render as a table, not show raw markdown source. Regression for the
        // "long comparison tables rendered un-rendered in the TUI" report.
        let mut st = MdState::new();
        assert!(
            render_line("对比 | HEAD | main", &mut st, plain_caps()).is_none(),
            "header buffered"
        );
        assert!(
            render_line("---|---|---", &mut st, plain_caps()).is_none(),
            "delimiter buffered"
        );
        assert!(
            render_line("代码 | 手动 | 调用", &mut st, plain_caps()).is_none(),
            "data buffered"
        );
        let out =
            render_line("下一段", &mut st, plain_caps()).expect("trailing line flushes table");
        assert!(
            !out.contains("---|---"),
            "raw delimiter must not leak: {out}"
        );
        assert!(
            out.contains("HEAD") && out.contains("代码"),
            "cells preserved: {out}"
        );
    }

    #[test]
    fn list_item_with_pipe_keeps_its_bullet_not_stolen_by_table_detection() {
        // Regression (code-review): a list item that mentions a literal `|`
        // (`- option A | option B`) is a LIST item, not a table row — it must keep
        // its bullet, not get buffered as a table and lose its marker.
        let mut st = MdState::new();
        let a = render_line("- option A | option B", &mut st, plain_caps())
            .expect("list item renders inline");
        assert!(
            a.contains('•') || a.contains('-'),
            "list marker preserved: {a}"
        );
        assert!(
            a.contains("option A") && a.contains("option B"),
            "content preserved: {a}"
        );
        // The `---|---` delimiter is NOT a list item, so real pipe-less tables still work.
        let mut st2 = MdState::new();
        assert!(render_line("H1 | H2", &mut st2, plain_caps()).is_none());
        assert!(
            render_line("---|---", &mut st2, plain_caps()).is_none(),
            "delimiter buffered, not a list item"
        );
        let out = render_line("done", &mut st2, plain_caps()).expect("flush");
        assert!(
            out.contains("H1") && out.contains("H2") && !out.contains("---|---"),
            "pipe-less header table still renders without leaking its delimiter: {out}"
        );
    }

    #[test]
    fn prose_with_literal_pipe_is_not_boxed() {
        // A lone prose line with a literal `|` splits into ≥2 cells and is
        // speculatively buffered, but with NO delimiter row the flush must render
        // it as inline prose, NOT draw a table box around it.
        let mut st = MdState::new();
        assert!(render_line("run foo | bar here", &mut st, plain_caps()).is_none());
        let out = render_line("next paragraph", &mut st, plain_caps()).expect("flush");
        assert!(
            !out.contains('┌'),
            "prose-with-pipe must not be boxed: {out}"
        );
        assert!(
            out.contains("foo") && out.contains("bar"),
            "prose preserved: {out}"
        );
    }

    #[test]
    fn flush_pipeless_rows_render_as_table_and_no_sep_renders_inline() {
        // Flush directly: pipe-less rows WITH a delimiter → table.
        let table = flush_aligned_table_with_width(
            &[
                "A | B".to_string(),
                "---|---".to_string(),
                "c | d".to_string(),
            ],
            plain_caps(),
            80,
        );
        assert!(
            !table.contains("---|---"),
            "raw delimiter must not leak: {table}"
        );
        assert!(!table.contains('│'), "table must be borderless: {table}");
        assert!(
            table.contains('A') && table.contains('d'),
            "cells preserved: {table}"
        );
        // No delimiter → NOT a table → inline, no box.
        let prose = flush_aligned_table_with_width(&["a | b".to_string()], plain_caps(), 80);
        assert!(
            !prose.contains('┌'),
            "no box without a delimiter row: {prose}"
        );
        assert!(prose.contains("a | b"), "raw inline preserved: {prose}");
    }

    /// Narrow terminal: table can't fit at natural widths → fall back to
    /// flat `header：cell` records so no cell content is lost. Mirrors the
    /// CC narrow-mode rendering the user requested.
    #[test]
    fn narrow_terminal_falls_back_to_flat_records() {
        let rows = vec![
            "| 能力 | AtomCode Air | Cursor | Copilot |".to_string(),
            "|------|--------------|--------|---------|".to_string(),
            "| 开源 | ✅ | ❌ | ❌ |".to_string(),
            "| 多语言运行 | ✅ Python+ | 🟡 | ❌ |".to_string(),
        ];
        // Tight budget — the natural box layout needs > 40 cols.
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 40);

        // Flat mode: no box-drawing characters anywhere.
        assert!(
            !out.contains('│'),
            "narrow output must not contain border │"
        );
        assert!(
            !out.contains('┌'),
            "narrow output must not contain top corner"
        );

        // Every cell value survives in full — no truncation.
        assert!(out.contains("AtomCode Air"));
        assert!(out.contains("Python+"));

        // Each header label appears once per data row.
        let count_neng_li = out.matches("能力").count();
        assert_eq!(
            count_neng_li, 2,
            "header `能力` should label both data rows"
        );
        let count_cursor = out.matches("Cursor").count();
        assert_eq!(
            count_cursor, 2,
            "header `Cursor` should label both data rows"
        );

        // Records are separated by a blank line.
        assert!(
            out.contains("\n\n"),
            "expected blank line between flat records"
        );
    }

    #[test]
    fn over_long_delimiter_does_not_paint_a_ghost_column() {
        // Weak models sometimes emit a delimiter row with one group too many.
        // The extra column has no data — it must not be drawn (GFM: content rows
        // define the shape).
        let rows = vec![
            "| 功能 | 说明 | 状态 |".to_string(),
            "| --- | --- | --- | --- |".to_string(),
            "| 补全 | 提示代码片段 | 已完成 |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        let top = out.lines().find(|line| line.contains("功能")).unwrap();
        assert_eq!(
            top.split_whitespace().count(),
            3,
            "3 columns expected, no ghost column:\n{out}"
        );
        assert!(out.contains("已完成"), "content preserved:\n{out}");
    }

    #[test]
    fn stray_trailing_empty_cell_is_trimmed_from_the_grid() {
        // A stray trailing `|` gives a data row an empty last cell. It must not
        // widen the table into a ghost column.
        let rows = vec![
            "| a | b | c |".to_string(),
            "| - | - | - |".to_string(),
            "| x | y | z |  |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        assert_eq!(
            out.lines()
                .find(|line| line.contains('a'))
                .unwrap()
                .split_whitespace()
                .count(),
            3,
            "trailing empty cell must be trimmed:\n{out}"
        );
    }

    #[test]
    fn word_wrap_cell_breaks_at_spaces_and_char_wraps_long_tokens() {
        // Prose breaks at word boundaries, never over the width.
        let lines = word_wrap_cell("Handles login and session tokens", 20);
        assert!(
            lines.iter().all(|l| crate::width::display_width(l) <= 20),
            "no line over width: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|l| !l.starts_with(' ') && !l.ends_with(' ')),
            "clean word breaks, no dangling spaces: {lines:?}"
        );
        assert_eq!(lines.join(" "), "Handles login and session tokens");

        // A single token wider than the column still degrades (char-wrapped),
        // never overflowing the box.
        let long = word_wrap_cell("parallel_edit_files", 8);
        assert!(long.len() >= 2, "oversized token must char-wrap: {long:?}");
        assert!(long.iter().all(|l| crate::width::display_width(l) <= 8));
        assert_eq!(long.concat(), "parallel_edit_files");

        // Empty content yields exactly one (empty) line so row height stays ≥ 1.
        assert_eq!(word_wrap_cell("", 10), vec![String::new()]);
    }

    #[test]
    fn moderately_wide_table_wraps_into_grid_not_flat() {
        // A table that's only *somewhat* too wide should keep its GRID (codex-style
        // middle tier): shrink the fat column + wrap its cell, rather than collapsing
        // to a flat vertical list.
        let rows = vec![
            "| Feature | Description | S |".to_string(),
            "|---|---|---|".to_string(),
            "| Auth | Handles login and session tokens | Y |".to_string(),
        ];
        // Natural row width ~50; give a 40-col budget so it overflows but can still
        // fit as a grid once the Description column is shrunk + wrapped.
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 40);

        // Grid survives — aligned rows remain and there are no flat labels.
        assert!(
            !out.contains('：'),
            "grid must be kept, not flattened:\n{out}"
        );
        // No content lost to the wrap.
        assert!(
            out.contains("Handles"),
            "start of wrapped cell present:\n{out}"
        );
        assert!(
            out.contains("tokens"),
            "end of wrapped cell present:\n{out}"
        );
        // Every rendered line fits the budget.
        for line in out.lines() {
            assert!(
                crate::width::display_width(line) <= 40,
                "line exceeds budget ({}> 40): {line:?}",
                crate::width::display_width(line)
            );
        }
        // Wrapping actually happened: header plus at least two physical body lines.
        assert!(
            out.lines().count() >= 3,
            "expected the description cell to wrap across lines:\n{out}"
        );
    }

    #[test]
    fn huge_cell_squeezed_thin_falls_back_to_flat_not_a_tall_box() {
        // The column floors let this fit as a grid, but the long prose cell would
        // wrap into a very tall stack in the thin column — the starvation guard
        // should prefer the flat key/value record instead of a 10+-line box row.
        let prose = "this is a fairly long note that keeps going and going with many \
                     small words on purpose to force the cell to wrap into a very tall \
                     stack of quite a lot of lines indeed well beyond what a single box \
                     row should ever hold in a scannable table";
        let rows = vec![
            "| Note | Detail |".to_string(),
            "|---|---|".to_string(),
            format!("| A | {prose} |"),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 30);
        assert!(
            !out.contains('│'),
            "a cell squeezed into a tall stack must drop to flat:\n{out}"
        );
        assert!(out.contains('：'), "must use key/value records:\n{out}");
        assert!(out.contains("tall stack"), "content preserved:\n{out}");
    }

    #[test]
    fn wrapped_grid_falls_back_to_flat_when_far_too_narrow() {
        // When even each column's token floor can't fit, the grid is abandoned for
        // the flat key/value fallback (no box characters survive).
        let rows = vec![
            "| Feature | Description | Status |".to_string(),
            "|---|---|---|".to_string(),
            "| Authentication | Handles login and session tokens | Complete |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 16);
        assert!(
            !out.contains('│'),
            "far-too-narrow must collapse to flat:\n{out}"
        );
        assert!(
            out.contains("Handles"),
            "content preserved in flat mode:\n{out}"
        );
    }

    #[test]
    fn one_column_viewport_does_not_become_unbounded() {
        let rows = vec![
            "| Feature | Status |".to_string(),
            "|---|---|".to_string(),
            "| Authentication | Complete |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 1);

        assert!(out.contains('：'), "must use the flat fallback:\n{out}");
        assert!(out.contains("Authentication") && out.contains("Complete"));
    }

    #[test]
    fn flat_mode_aligns_labels_into_a_column() {
        // The flat key/value fallback right-pads labels so the fullwidth colons
        // line up (codex-style record), instead of ragged `Session：`/`Detected：`.
        let rows = vec![
            "| Session | Detected |".to_string(),
            "|---|---|".to_string(),
            "| gallery build from this long thread | 7 |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 20);
        assert!(!out.contains('│'), "should be flat at 20 cols:\n{out}");
        // The shorter label "Session" is padded to the width of "Detected" so its
        // colon sits at the same column.
        let colon_cols: Vec<usize> = out
            .lines()
            .filter(|l| l.contains('：'))
            .map(|l| {
                let idx = l.find('：').unwrap();
                crate::width::display_width(&l[..idx])
            })
            .collect();
        assert!(colon_cols.len() >= 2, "expected ≥2 labelled lines:\n{out}");
        assert!(
            colon_cols.iter().all(|&c| c == colon_cols[0]),
            "all colons must align to the same column, got {colon_cols:?}:\n{out}"
        );
    }

    /// Threshold transition: the same table in a slightly different
    /// terminal width should switch modes cleanly.
    #[test]
    fn flat_mode_kicks_in_when_natural_width_exceeds_budget() {
        let rows = vec![
            "| A | B | C |".to_string(),
            "|---|---|---|".to_string(),
            "| short | also short | x |".to_string(),
        ];
        // Natural width ~ 1 + (5+3) + (10+3) + (1+3) = 26.
        let wide = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        assert!(!wide.contains('：'), "80 cols should render as a grid");

        let narrow = flush_aligned_table_with_width(&rows, plain_caps(), 20);
        assert!(narrow.contains('：'), "20 cols should fall back to flat");
    }

    /// Pre-drawn Unicode box-drawing tables (the `┌─┬─┐ │ ├─┼─┤ └─┴─┘`
    /// shape some weak models emit instead of `|`-form markdown) must
    /// route through the same flat-mode-aware flush path: at narrow widths
    /// they collapse to `header：cell` records — no box characters survive.
    /// This is the macOS-overflow regression captured in the screenshot.
    #[test]
    fn box_drawing_table_collapses_to_flat_when_narrow() {
        let mut st = MdState::new();
        let lines = [
            "┌──────────────┬──────────────────────────────────────────┐",
            "│ 场景         │ 作用                                     │",
            "├──────────────┼──────────────────────────────────────────┤",
            "│ 多文件并行编辑 │ parallel_edit_files 工具触发时分发给子智能体 │",
            "├──────────────┼──────────────────────────────────────────┤",
            "│ 弹性预算控制 │ 每个 SubAgent 有初始 4 轮对话预算          │",
            "└──────────────┴──────────────────────────────────────────┘",
            "", // boundary line triggers flush
        ];
        let mut out = String::new();
        for line in &lines {
            if let Some(r) = render_line_with_width(line, &mut st, plain_caps(), 30) {
                out.push_str(&r);
                out.push('\n');
            }
        }
        // Narrow → flat-mode kicks in. No box corners survive.
        assert!(
            !out.contains('┌') && !out.contains('└'),
            "narrow box-drawing table must collapse to flat:\n{out}"
        );
        // Each header label appears once per data row (2 data rows here).
        assert_eq!(
            out.matches("场景").count(),
            2,
            "header `场景` should label each data record:\n{out}"
        );
        assert_eq!(out.matches("作用").count(), 2);
        // Cell content survives in full — no truncation.
        assert!(out.contains("parallel_edit_files"));
        assert!(out.contains("初始 4 轮"));
    }

    /// Wide terminal: pre-drawn input is normalized into the same safe
    /// borderless table used for ordinary markdown.
    #[test]
    fn box_drawing_table_re_renders_borderless_when_fits() {
        let mut st = MdState::new();
        let lines = [
            "┌─────┬─────┐",
            "│ a   │ b   │",
            "├─────┼─────┤",
            "│ 1   │ 2   │",
            "└─────┴─────┘",
            "",
        ];
        let mut out = String::new();
        for line in &lines {
            if let Some(r) = render_line_with_width(line, &mut st, plain_caps(), 80) {
                out.push_str(&r);
                out.push('\n');
            }
        }
        assert!(
            !out.contains('：'),
            "wide terminal should keep a grid:\n{out}"
        );
        assert!(!out.contains('┌') && !out.contains('└') && !out.contains('│'));
        assert!(out.contains("a") && out.contains("2"));
    }

    /// False-positive guard: a paragraph whose first character happens to
    /// be `├` (or any junction) but has surrounding prose must NOT be
    /// pulled into the box-table buffer. The border-row matcher requires
    /// the entire trimmed line to consist of box-drawing chars + spaces.
    #[test]
    fn box_drawing_detection_does_not_swallow_prose_with_stray_box_char() {
        let mut st = MdState::new();
        // Prose that starts with `├` followed by regular words. Real-world
        // probability is near-zero but the guard matters.
        let line = "├ hello, this is not a table line";
        let out = render_line_with_width(line, &mut st, plain_caps(), 80);
        // Must render inline (Some), not buffer (None).
        assert!(out.is_some(), "prose with stray junction must not buffer");
        assert!(st.table_buf.is_empty(), "table_buf must stay empty");
    }

    #[test]
    fn mdstate_default_has_empty_code_buf() {
        let s = MdState::new();
        assert!(s.code_buf.is_empty(), "code_buf must start empty");
        assert!(!s.in_code_block, "in_code_block must start false");
    }

    #[test]
    fn mdstate_reset_clears_code_buf() {
        let mut s = MdState::new();
        s.code_buf.push("dirty".into());
        s.in_code_block = true;
        s.reset();
        assert!(s.code_buf.is_empty(), "reset must clear code_buf");
        assert!(!s.in_code_block, "reset must clear in_code_block");
    }

    #[test]
    fn fence_open_with_lang_tag_buffers_lines_without_emit() {
        // Under plan-0 the lang tag is ignored entirely (no language-
        // dependent path remains). We only assert that the fence
        // transitions state into `in_code_block` and accumulates body
        // lines silently until the close fence flushes.
        let mut st = MdState::new();
        assert!(render_line("```rust", &mut st, caps()).is_none());
        assert!(st.in_code_block);

        assert!(render_line("let x = 1;", &mut st, caps()).is_none());
        assert!(render_line("let y = 2;", &mut st, caps()).is_none());
        assert_eq!(st.code_buf.len(), 2);
    }

    #[test]
    fn fence_close_flushes_buffered_block_as_one_chunk() {
        let mut st = MdState::new();
        assert!(render_line("```rust", &mut st, plain_caps()).is_none());
        assert!(render_line("let x = 1;", &mut st, plain_caps()).is_none());
        assert!(render_line("let y = 2;", &mut st, plain_caps()).is_none());

        // Close fence -> highlighted block returned; state reset.
        let out = render_line("```", &mut st, plain_caps()).expect("close fence flushes");
        assert!(out.contains("let x = 1;"));
        assert!(out.contains("let y = 2;"));
        // Output is a single multi-line string (two indented lines + newline between).
        assert!(out.split('\n').count() >= 2);
        // State is reset for the next block.
        assert!(!st.in_code_block);
        assert!(st.code_buf.is_empty());
    }

    #[test]
    fn fence_close_with_colors_emits_plain_indent_no_ansi() {
        // Plan-0: even with `caps.colors == true`, code-block flush
        // emits the body with a 2-space indent and zero ANSI. See
        // `highlight::highlight_block`'s doc for why per-token colour
        // was dropped (Terminal.app selection-overlay readability).
        let mut st = MdState::new();
        render_line("```rust", &mut st, caps());
        render_line("fn main() {}", &mut st, caps());
        let out = render_line("```", &mut st, caps()).unwrap();
        assert!(
            !out.contains('\x1b'),
            "code-block output must contain zero ANSI under plan-0, got: {:?}",
            out
        );
        assert!(
            out.contains("  fn main() {}"),
            "indent + body preserved: {:?}",
            out
        );
    }

    #[test]
    fn fence_close_with_no_color_caps_emits_plain_indent_no_ansi() {
        let mut st = MdState::new();
        render_line("```rust", &mut st, plain_caps());
        render_line("let x = 1;", &mut st, plain_caps());
        let out = render_line("```", &mut st, plain_caps()).unwrap();
        assert!(out.contains("  let x = 1;"));
        assert!(
            !out.contains('\x1b'),
            "plain_caps must emit zero ANSI, got: {:?}",
            out
        );
    }

    #[test]
    fn fence_open_with_no_lang_tag_buffers() {
        let mut st = MdState::new();
        assert!(render_line("```", &mut st, caps()).is_none());
        assert!(st.in_code_block);
    }

    #[test]
    fn finalize_emits_unclosed_code_block_as_fallback() {
        // Stream cuts off before close fence — finalize must still emit
        // the buffered body, otherwise the user's last few lines vanish.
        let mut st = MdState::new();
        render_line("```rust", &mut st, caps());
        render_line("let x = 1;", &mut st, caps());
        render_line("let y = 2;", &mut st, caps());
        // No close fence.

        let out = finalize(&mut st, caps()).expect("unclosed block must emit something");
        // Under plan-0 the body is emitted verbatim (2-space indent, no
        // ANSI), so the source lines are contiguous and substring-checkable
        // directly without bouncing through `plain_caps()`.
        assert!(out.contains("let x = 1;"), "got: {:?}", out);
        assert!(out.contains("let y = 2;"), "got: {:?}", out);
        assert!(st.code_buf.is_empty());
        assert!(!st.in_code_block);
    }

    #[test]
    fn finalize_with_no_active_block_returns_none() {
        // Existing behavior: no buffered table / code → returns None.
        let mut st = MdState::new();
        assert!(finalize(&mut st, caps()).is_none());
    }

    // ─── strip_md_for_width vs render_inline alignment ───
    //
    // For table column-width measurement to be correct, the visible string
    // strip_md_for_width returns has to be exactly what render_inline
    // ultimately paints (after the SGR escapes are removed). Any divergence
    // pads the row by the wrong number of cells and bends the right border
    // out of alignment with neighbouring rows.

    /// Reported bug: glob row in the tools table painted ~2 cells past the
    /// other rows' right border because `` `src/**/*.ts` `` had its inner
    /// `**` stripped as bold markers. Pin: `**` inside a code span is
    /// literal, never stripped.
    #[test]
    fn strip_md_for_width_keeps_double_star_inside_inline_code() {
        assert_eq!(strip_md_for_width("`src/**/*.ts`"), "src/**/*.ts");
    }

    /// Companion to the bug above: `*italic*` is rendered as the inner text
    /// only (single `*` stripped by render_inline). Strip must do the same,
    /// otherwise plain_w over-counts by 2 cells per italic span.
    #[test]
    fn strip_md_for_width_strips_single_star_italic_markers() {
        assert_eq!(strip_md_for_width("*italic*"), "italic");
    }

    #[test]
    fn strip_md_for_width_strips_double_star_bold_markers() {
        assert_eq!(strip_md_for_width("**bold**"), "bold");
    }

    #[test]
    fn strip_md_for_width_strips_backtick_code_markers() {
        assert_eq!(strip_md_for_width("`code`"), "code");
    }

    /// Unclosed markers stay verbatim — matches render_inline's "no closer
    /// found, dump the marker as literal" fallback, so column-width math
    /// keeps treating them as content.
    #[test]
    fn strip_md_for_width_keeps_unclosed_markers_verbatim() {
        assert_eq!(strip_md_for_width("**unclosed"), "**unclosed");
        assert_eq!(strip_md_for_width("*unclosed"), "*unclosed");
        assert_eq!(strip_md_for_width("`unclosed"), "`unclosed");
    }

    /// The literal reported regression input: a CJK cell containing an
    /// inline-code span whose inner text has `**` in it. Pre-fix
    /// `strip_md_for_width` returned a string two cells short of what
    /// `render_inline` paints; post-fix they match exactly.
    #[test]
    fn strip_md_for_width_matches_render_visible_width_on_glob_cell() {
        use crate::width::display_width;
        let md = "文件名通配符匹配（如 `src/**/*.ts`）";
        let stripped = strip_md_for_width(md);
        let rendered = render_inline(md, caps());
        // Visible width of render = bytes of render minus SGR escape bytes.
        // Easier: re-strip the rendered output by walking past `\x1b[...m`
        // chunks; then assert it equals stripped.
        let mut visible = String::new();
        let bytes = rendered.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b {
                while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // consume the alpha terminator (m / K / etc.)
                }
                continue;
            }
            let next = rendered[i..]
                .char_indices()
                .nth(1)
                .map(|(idx, _)| idx + i)
                .unwrap_or(rendered.len());
            visible.push_str(&rendered[i..next]);
            i = next;
        }
        assert_eq!(stripped, visible, "strip must equal render-visible");
        assert_eq!(display_width(&stripped), display_width(&visible));
    }
}
