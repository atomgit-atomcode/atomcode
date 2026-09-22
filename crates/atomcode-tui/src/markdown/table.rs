//! GFM tables, laid out as a full box grid.
//!
//! A table is drawn the way most readers expect one: an outer box, a vertical
//! border between the columns, and a horizontal rule between every pair of rows
//! (`┌┬┐ │…│ ├┼┤ └┴┘`). The header is told from the body by its bold text, not by
//! a heavier rule. This replaces the earlier borderless form — segmented rules
//! and no verticals — which rendered on more hosts but read as a list of
//! dividers; the compatibility that form bought is kept by other means (below).
//!
//! The column maths are `atomcode-tuix`'s, and so are the three tiers, which the
//! box did not change: the **natural grid** at the columns' own widths; a
//! **wrapped grid** whose cells fold to shrunk widths when the natural one is too
//! wide (a tall cell makes a several-line row, the short columns blank on the
//! lines they have run out of); and, when even that squeezes the values into
//! unreadable strips, flat `header：value` records. Those tiers were beaten out of
//! real model output (pipeless tables, pre-drawn box tables, CJK that shifts
//! every border after it).
//!
//! **Compatibility, three ways.** A cell's visible width and text both come from
//! rendering it once — one parser, unlike tuix's ANSI-plus-strip pair that
//! drifted into a reported misalignment — so CJK columns line up. The box glyphs
//! are all in the `ascii_for` downgrade table, so a non-Unicode terminal gets
//! `+ | -` for free on the way out. And the fit budget reserves a slack cell per
//! border ([`chrome_width`]) against the one width it cannot see: box glyphs are
//! East Asian *Ambiguous*, so a CJK host may draw them two cells wide — the very
//! error that first drove tuix to drop the box.
//!
//! Everything above [`render`] works on strings and widths only; the mapping to
//! styles happens in the last step of each tier.

use crate::frame::{Line, Span, Style};
use crate::width;

use super::{fence, inline, wrap_spans};

/// Cells are padded by one space on each side; columns and the table's edges are
/// separated by a vertical border. The column maths are `atomcode-tuix`'s; the
/// box is new — the earlier segmented-rule form rendered on more hosts but read
/// as a list of dividers rather than a table.
const PAD: usize = 1;
/// A column is never given the last cell of the viewport. Besides looking less
/// cramped, a line that exactly touches the right edge is the one a terminal is
/// most likely to wrap on its own.
const RIGHT_GUARD: usize = 1;

// The box, in light box-drawing glyphs. Every one of these is in the `ascii_for`
// downgrade table (`caps.rs`), so a non-Unicode terminal gets `+ | -` for free
// as the lines leave the renderer — the table needs no ASCII branch of its own.
const VERT: &str = "│";
const HORIZ: &str = "─";
const TOP_L: &str = "┌";
const TOP_MID: &str = "┬";
const TOP_R: &str = "┐";
const MID_L: &str = "├";
const MID_X: &str = "┼";
const MID_R: &str = "┤";
const BOT_L: &str = "└";
const BOT_MID: &str = "┴";
const BOT_R: &str = "┘";

/// The chrome (everything that is not cell content) the box costs `ncols`
/// columns, for the fit budget: the `ncols + 1` vertical borders, two pad cells
/// per column, and a slack of one extra cell per border.
///
/// That slack is the compatibility hedge. `│ ┼ ┬ …` are East Asian *Ambiguous*,
/// so a CJK-configured host may draw each one two cells wide while the layout —
/// like every terminal-width measurement — counts it as one. Budgeting the
/// borders at their worst case keeps the row inside the terminal there too; on
/// the common terminal, where they are one cell, it just leaves a little air on
/// the right. It is the width the module's ancestors dropped the box to avoid.
fn chrome_width(ncols: usize) -> usize {
    let borders = ncols + 1;
    borders + PAD * 2 * ncols + borders
}

/// Is this line a table row? Returns its canonical `|` form when it is.
///
/// Two shapes count. A line containing a `|` is a row — GFM tables need not be
/// delimited at both edges, and models emit both forms. Buffering is only ever a
/// *candidacy*: a delimiter row among the buffered lines is what makes the block
/// a table, so a paragraph that merely mentions a pipe is handed back as prose.
///
/// Counting the pipe rather than the cells is what lets a **one-column** table
/// through. `| 只有一列 |` splits into a single cell, and requiring two or more
/// read every row of the table as "not a row", so the whole thing came out as
/// literal pipes. Nothing is lost by the wider test: `split_row` only ever
/// splits on a `|`, so "two or more cells" already implied "contains a `|`".
///
/// A row already drawn in box characters (`│ a │ b │`) is converted instead of
/// being left to fall through as shattered text, which is what happens when a
/// model imitates the table we ourselves drew.
pub(super) fn row(line: &str) -> Option<String> {
    if let Some(converted) = box_drawing_row(line) {
        return Some(converted);
    }
    line.contains('|').then(|| line.to_string())
}

/// A pre-drawn box-drawing row, as the equivalent `|` row.
///
/// Two shapes. A **data row** starts with a vertical (`│`): every vertical
/// becomes a cell separator. A **border row** is made of nothing but box
/// characters and spaces: its junctions become `|` and its horizontals `-`,
/// which is exactly the `|---|---|` delimiter the layout already recognises —
/// that conversion is why a table someone drew renders as a table rather than as
/// text with broken borders.
///
/// Those two characters are named in the doc comment rather than in the code,
/// because the code reads the **code point range** instead: the whole
/// box-drawing block, double lines (`═`, `║`) included, since a model drawing a
/// table with double lines is still drawing a table. Everything here is *read*,
/// never drawn — nothing from this table reaches the output — so the layering
/// ratchet, which counts decorative glyphs a module *writes*, has nothing to
/// count.
fn box_drawing_row(line: &str) -> Option<String> {
    let first = line.chars().next()?;
    if is_box_vertical(first) {
        return Some(
            line.chars()
                .map(|c| if is_box_vertical(c) { '|' } else { c })
                .collect(),
        );
    }
    if !is_box(first) {
        return None;
    }
    // The all-box guard is what keeps a paragraph that merely starts with a
    // left tee from being eaten: it has letters in it.
    if !line.chars().all(|c| is_box(c) || c == ' ') {
        return None;
    }
    Some(
        line.chars()
            .map(|c| match c {
                c if is_box_horizontal(c) => '-',
                c if is_box(c) => '|',
                other => other,
            })
            .collect(),
    )
}

/// Any character of the box-drawing block, U+2500–U+257F (`─` `│` `┌` `┼` …).
fn is_box(c: char) -> bool {
    ('\u{2500}'..='\u{257f}').contains(&c)
}

/// A horizontal one: U+2500 `─` or U+2550 `═`.
fn is_box_horizontal(c: char) -> bool {
    c == '\u{2500}' || c == '\u{2550}'
}

/// A vertical one: U+2502 `│` or U+2551 `║`.
fn is_box_vertical(c: char) -> bool {
    c == '\u{2502}' || c == '\u{2551}'
}

/// Lay a buffered table block out at `w`.
///
/// `None` when the block turns out not to be a table: a real GFM table requires
/// a delimiter row, and detection buffers any pipe-splitting line. The caller
/// then reads the buffered lines as ordinary markdown, so a paragraph with a
/// literal `|` comes out as prose rather than as a box.
pub(super) fn render(rows: &[String], w: u16, base: Style) -> Option<Vec<Line>> {
    let parsed: Vec<Vec<String>> = rows.iter().map(|r| split_row(r)).collect();
    if parsed.len() < 2 || !parsed.iter().any(|r| is_separator(r)) {
        return None;
    }
    let ncols = effective_ncols(&parsed);
    if ncols == 0 {
        // Nothing but delimiter rows: there is no table to draw. `None` rather
        // than an empty block, so the lines are not silently dropped.
        return None;
    }
    let mut natural = vec![0usize; ncols];
    for row in &parsed {
        if is_separator(row) {
            continue;
        }
        for (j, cell) in row.iter().enumerate().take(ncols) {
            natural[j] = natural[j].max(visible_width(cell, base));
        }
    }
    let natural_row = natural.iter().sum::<usize>() + chrome_width(ncols);
    let budget = (w as usize).saturating_sub(RIGHT_GUARD);
    if natural_row > budget {
        // Middle tier: keep the grid by shrinking wide columns and folding their
        // cells, so a table that is only somewhat too wide does not collapse
        // straight to a vertical list.
        if let Some(shrunk) = fit_columns(&parsed, &natural, ncols, budget, base) {
            if !too_starved(&parsed, &shrunk, base) {
                return Some(wrapped_grid(&parsed, &shrunk, base));
            }
        }
        return Some(flat(&parsed, w, base));
    }
    Some(aligned(&parsed, &natural, base))
}

/// Split a row on `|`, honouring:
///
/// * `` ` ``-spans, so the pipes inside a Rust closure `|a, b|`, a bash pipe
///   `cat | grep` or a type union `int | str` stay inside their cell;
/// * `\|`, the markdown escape for a literal pipe;
/// * a leading and a trailing `|`, which are delimiters rather than cells.
///
/// Cells come back trimmed. Backticks are preserved: the inline renderer is what
/// strips them, and it is also what measures, so nothing here has to guess at
/// the visible text.
fn split_row(line: &str) -> Vec<String> {
    let mut cells: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_code = false;
    let mut chars = line.char_indices();
    if line.starts_with('|') {
        chars.next();
    }
    while let Some((i, c)) = chars.next() {
        match c {
            '`' => {
                in_code = !in_code;
                current.push(c);
            }
            '\\' if !in_code => match chars.next() {
                Some((_, '|')) => current.push('|'),
                Some((_, other)) => {
                    current.push('\\');
                    current.push(other);
                }
                None => current.push('\\'),
            },
            '|' if !in_code => {
                cells.push(current.trim().to_string());
                current = String::new();
                // A trailing delimiter ends the row; anything after it is not a
                // cell.
                #[allow(
                    clippy::string_slice,
                    reason = "`i` is an index from `line.char_indices()` above, so `i + 1` is the end of the one-byte `|` and both are char boundaries"
                )]
                if line[i + 1..].trim().is_empty() {
                    break;
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() || cells.is_empty() {
        cells.push(current.trim().to_string());
    }
    cells
}

/// A GFM delimiter row: every cell is made of `-`, `:` and spaces only. One
/// predicate, shared by every tier, so "is this the separator?" cannot drift
/// between the grid and the flat path.
fn is_separator(row: &[String]) -> bool {
    row.iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
}

/// The table's real column count: the widest content row, less any trailing
/// column that is empty in every content row. Models do emit a stray extra `|`
/// or an over-long delimiter (`|---|---|---|`), and counting the separator row's
/// width then paints a ghost column of blanks down the right edge. Always keeps
/// at least one column.
fn effective_ncols(parsed: &[Vec<String>]) -> usize {
    let content: Vec<&Vec<String>> = parsed.iter().filter(|r| !is_separator(r)).collect();
    let mut n = content.iter().map(|r| r.len()).max().unwrap_or(0);
    while n > 1
        && content
            .iter()
            .all(|r| r.get(n - 1).is_none_or(|c| c.trim().is_empty()))
    {
        n -= 1;
    }
    n
}

/// One cell as it will be drawn, at `base`.
fn cell_line(cell: &str, base: Style) -> Line {
    Line::from_spans(inline(cell, base))
}

/// Cells a cell will occupy once drawn — the renderer's answer, not a second
/// parser's.
fn visible_width(cell: &str, base: Style) -> usize {
    cell_line(cell, base).width()
}

/// Visible text of a cell — again the renderer's answer.
fn visible_plain(cell: &str, base: Style) -> String {
    cell_line(cell, base).plain()
}

/// Width of the longest whitespace-delimited token: a column's shrink floor, so
/// a column is not squeezed narrower than its widest unbreakable word. A run of
/// CJK has no interior space and counts as one token; the caller caps the floor,
/// and CJK reads acceptably char-wrapped, so that is fine.
fn longest_token_width(plain: &str) -> usize {
    plain
        .split_whitespace()
        .map(width::str_width)
        .max()
        .unwrap_or(0)
}

/// Fold plain cell text to `max` cells: pack whole words per line, and only
/// char-wrap a single token too wide to fit at all — so prose breaks at spaces
/// while a long identifier or a space-less CJK run still degrades gracefully.
/// Always returns at least one (possibly empty) line.
fn word_wrap(plain: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![plain.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for token in plain.split_whitespace() {
        let tw = width::str_width(token);
        if tw > max {
            // Cannot fit on any line: break it by grapheme, and keep the
            // trailing partial as the current line so following words pack on.
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            let mut chunks = width::wrap(token, max);
            if let Some(last) = chunks.pop() {
                lines.extend(chunks);
                cur_w = width::str_width(&last);
                cur = last;
            }
            continue;
        }
        let sep = usize::from(!cur.is_empty());
        if cur_w + sep + tw > max {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if !cur.is_empty() {
            cur.push(' ');
            cur_w += 1;
        }
        cur.push_str(token);
        cur_w += tw;
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Per-column widths that fit `budget` with the gutters and padding included,
/// or `None` when the columns cannot be squeezed that far even at their floors —
/// then the caller uses the flat fallback.
///
/// Floors are each column's longest token clamped to `[MIN_COL, TOKEN_CAP]`:
/// identifiers are not broken mid-word, while one enormous token cannot veto the
/// whole grid.
fn fit_columns(
    parsed: &[Vec<String>],
    natural: &[usize],
    ncols: usize,
    budget: usize,
    base: Style,
) -> Option<Vec<usize>> {
    const MIN_COL: usize = 4;
    const TOKEN_CAP: usize = 16;
    let content_budget = budget.checked_sub(chrome_width(ncols))?;
    if content_budget == 0 {
        return None;
    }
    let mut floors = vec![MIN_COL; ncols];
    for row in parsed {
        if is_separator(row) {
            continue;
        }
        for (j, cell) in row.iter().enumerate().take(ncols) {
            let token = longest_token_width(&visible_plain(cell, base));
            floors[j] = floors[j].max(token.min(TOKEN_CAP));
        }
    }
    // A column already narrower than its floor stays where it is.
    for (floor, nat) in floors.iter_mut().zip(natural) {
        *floor = (*floor).min(*nat);
    }
    if floors.iter().sum::<usize>() > content_budget {
        return None;
    }
    // Shave the widest still-shrinkable column until the row fits.
    let mut cols = natural.to_vec();
    let mut total: usize = cols.iter().sum();
    while total > content_budget {
        let mut widest: Option<usize> = None;
        for j in 0..ncols {
            if cols[j] > floors[j] && widest.is_none_or(|b| cols[j] > cols[b]) {
                widest = Some(j);
            }
        }
        // `None` is unreachable: the floor sum fits, so a shave is always available.
        let j = widest?;
        cols[j] -= 1;
        total -= 1;
    }
    Some(cols)
}

/// Whether the folded grid would be so cramped that flat records read better.
///
/// Fragmented tokens, several cramped cells, or a catastrophically tall row all
/// count against the grid; one exceptional row is not enough to flatten a large
/// otherwise-useful table, which is why the threshold scales with the body.
fn too_starved(parsed: &[Vec<String>], col_widths: &[usize], base: Style) -> bool {
    const MIN_SCANNABLE_WIDTH: usize = 12;
    const CRAMPED_CELL_LINES: usize = 4;
    const CATASTROPHIC_CELL_LINES: usize = 7;

    // The first content row is the header; readability is judged on the body.
    let body: Vec<&Vec<String>> = parsed.iter().filter(|r| !is_separator(r)).skip(1).collect();
    if body.is_empty() {
        return false;
    }
    let affected = body
        .iter()
        .filter(|row| {
            let mut cramped = 0usize;
            let mut catastrophic = false;
            let mut fragmented = false;
            let mut tallest = 1usize;
            for (j, &cw) in col_widths.iter().enumerate() {
                let cell = row.get(j).map(String::as_str).unwrap_or("");
                let plain = visible_plain(cell, base);
                let height = word_wrap(&plain, cw).len();
                tallest = tallest.max(height);
                cramped += usize::from(height >= CRAMPED_CELL_LINES);
                catastrophic |= cw < MIN_SCANNABLE_WIDTH && height >= CATASTROPHIC_CELL_LINES;
                fragmented |= cw < MIN_SCANNABLE_WIDTH
                    && plain.split_whitespace().any(|t| width::str_width(t) > cw);
            }
            fragmented || cramped >= 2 || catastrophic || tallest > 8
        })
        .count();
    let threshold = if body.len() == 1 {
        1
    } else {
        2.max(body.len().div_ceil(3))
    };
    affected >= threshold
}

/// A horizontal border across all columns: `left`, then a run of `─` for each
/// column (its content width plus its two pad cells), joined by `junction`, then
/// `right`. Top (`┌┬┐`), the header/row separators (`├┼┤`) and the bottom
/// (`└┴┘`) differ only in those three glyphs. Drawn in one string so the whole
/// rule shares one style and one downgrade scan.
fn border_line(col_widths: &[usize], left: &str, junction: &str, right: &str) -> Line {
    let mut s = String::from(left);
    for (j, &cw) in col_widths.iter().enumerate() {
        if j > 0 {
            s.push_str(junction);
        }
        s.push_str(&HORIZ.repeat(cw + PAD * 2));
    }
    s.push_str(right);
    Line::styled(s, fence())
}

/// The separator below a row: the bottom border after the last row, an inner
/// `├┼┤` rule between every other pair — the full grid the reader chose.
fn separator_below(i: usize, rows: usize, col_widths: &[usize]) -> Line {
    if i + 1 == rows {
        border_line(col_widths, BOT_L, BOT_MID, BOT_R)
    } else {
        border_line(col_widths, MID_L, MID_X, MID_R)
    }
}

/// Rows of a table that are not delimiter rows.
fn data_rows(parsed: &[Vec<String>]) -> Vec<&Vec<String>> {
    parsed.iter().filter(|r| !is_separator(r)).collect()
}

/// One content cell inside its borders: left pad, the cell, the fill to the
/// column width, right pad. The caller draws the `│` between cells; this is just
/// what sits in one column.
fn padded_cell(spans: &mut Vec<Span>, content: Vec<Span>, used: usize, cw: usize, base: Style) {
    let fill = cw.saturating_sub(used);
    spans.push(Span::styled(" ".repeat(PAD), base));
    spans.extend(content);
    if fill > 0 {
        spans.push(Span::styled(" ".repeat(fill), base));
    }
    spans.push(Span::styled(" ".repeat(PAD), base));
    spans.push(Span::styled(VERT.to_string(), fence()));
}

/// The natural grid: a full box, every row at the columns' natural widths, the
/// header set in bold (the body's colour, not a highlight), and `├┼┤` rules
/// between every pair of rows.
fn aligned(parsed: &[Vec<String>], col_widths: &[usize], base: Style) -> Vec<Line> {
    let rows = data_rows(parsed);
    let mut out = vec![border_line(col_widths, TOP_L, TOP_MID, TOP_R)];
    for (i, row) in rows.iter().enumerate() {
        let style = if i == 0 { base.bold() } else { base };
        let mut spans = vec![Span::styled(VERT.to_string(), fence())];
        for (j, &cw) in col_widths.iter().enumerate() {
            let cell = cell_line(row.get(j).map(String::as_str).unwrap_or(""), style);
            let used = cell.width();
            padded_cell(&mut spans, cell.spans, used, cw, base);
        }
        out.push(Line::from_spans(spans));
        out.push(separator_below(i, rows.len(), col_widths));
    }
    out
}

/// The folded grid: cells wrapped to shrunk column widths, tallest cell sets the
/// row's height.
///
/// Cells are drawn as plain text here, not as their inline spans. Folding styled
/// spans means re-padding each wrapped line to keep the columns aligned, and the
/// gain — bold inside a table that is already degraded — is not worth the
/// alignment risk. The visible text is identical either way, which is all this
/// tier promises.
fn wrapped_grid(parsed: &[Vec<String>], col_widths: &[usize], base: Style) -> Vec<Line> {
    let ncols = col_widths.len();
    let rows = data_rows(parsed);
    let mut out = vec![border_line(col_widths, TOP_L, TOP_MID, TOP_R)];
    for (i, row) in rows.iter().enumerate() {
        let style = if i == 0 { base.bold() } else { base };
        let wrapped: Vec<Vec<String>> = (0..ncols)
            .map(|j| {
                let cell = row.get(j).map(String::as_str).unwrap_or("");
                word_wrap(&visible_plain(cell, base), col_widths[j])
            })
            .collect();
        // A tall cell makes the row several lines; the shorter columns are blank
        // on the lines they have run out of, so their borders still line up.
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for k in 0..height {
            let mut spans = vec![Span::styled(VERT.to_string(), fence())];
            for (j, &cw) in col_widths.iter().enumerate() {
                let text = wrapped[j].get(k).map(String::as_str).unwrap_or("");
                let used = width::str_width(text);
                padded_cell(
                    &mut spans,
                    vec![Span::styled(text.to_string(), style)],
                    used,
                    cw,
                    base,
                );
            }
            out.push(Line::from_spans(spans));
        }
        out.push(separator_below(i, rows.len(), col_widths));
    }
    out
}

/// The floor: one `header：value` line per cell, records separated by a blank
/// line, labels right-padded so the values line up into a column. No content is
/// dropped — the lines are folded to `w` on the way out, which is this tier's
/// whole justification.
fn flat(parsed: &[Vec<String>], w: u16, base: Style) -> Vec<Line> {
    let ncols = effective_ncols(parsed);
    let mut rows = data_rows(parsed);
    // The first content row is the header when the block has a delimiter row,
    // which `render` has already established.
    let headers: Vec<String> = rows.first().map(|h| (*h).clone()).unwrap_or_default();
    if !headers.is_empty() {
        rows.remove(0);
    }
    let label_w = headers
        .iter()
        .map(|h| visible_width(h, base))
        .max()
        .unwrap_or(0);
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(Line::empty());
        }
        for j in 0..ncols {
            let mut spans: Vec<Span> = Vec::new();
            if let Some(header) = headers.get(j) {
                let label = cell_line(header, base.bold());
                let pad = label_w.saturating_sub(label.width());
                spans.extend(label.spans);
                if pad > 0 {
                    spans.push(Span::styled(" ".repeat(pad), base));
                }
                spans.push(Span::styled("：", base));
            }
            spans.extend(cell_line(row.get(j).map(String::as_str).unwrap_or(""), base).spans);
            out.extend(wrap_spans(&spans, w, "", base));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rendered text, and the width of every line — the two things that are
    /// visible on a terminal.
    fn drawn(rows: &[&str], w: u16) -> Vec<String> {
        render(
            &rows.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
            w,
            Style::new(),
        )
        .expect("not recognised as a table")
        .iter()
        .map(Line::plain)
        .collect()
    }

    fn widths(rows: &[&str], w: u16) -> Vec<usize> {
        render(
            &rows.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
            w,
            Style::new(),
        )
        .expect("not recognised as a table")
        .iter()
        .map(Line::width)
        .collect()
    }

    /// A full box is drawn around the grid, so not every line is a row: the
    /// borders (`┌├└`) are filtered out and only the content rows (`│ … │`) kept.
    fn rows_of(out: &[String]) -> Vec<String> {
        out.iter()
            .filter(|l| l.trim_start().starts_with('│'))
            .cloned()
            .collect()
    }

    /// The cell at which `mark` starts — the thing that has to agree across rows.
    fn column_at(row: &str, mark: &str) -> usize {
        #[allow(
            clippy::string_slice,
            reason = "`str::find` returns a byte offset that is always a char boundary"
        )]
        width::str_width(&row[..row.find(mark).expect("mark not in row")])
    }

    fn cells(line: &str) -> Vec<String> {
        split_row(line)
    }

    // ─── the box ───

    #[test]
    fn a_table_is_drawn_as_a_full_grid() {
        let out = drawn(&["| a | b |", "|---|---|", "| 1 | 2 |"], 40);
        // Top border, corners and a top junction.
        assert!(
            out[0].starts_with('┌') && out[0].contains('┬') && out[0].ends_with('┐'),
            "top border: {:?}",
            out[0]
        );
        // Every content row is bordered left and right, with a vertical between the
        // two columns — three verticals for a two-column table.
        let body: Vec<&String> = out.iter().filter(|l| l.starts_with('│')).collect();
        assert!(
            body.iter().all(|l| l.ends_with('│')),
            "rows closed on the right: {out:?}"
        );
        assert!(
            body.iter().all(|l| l.matches('│').count() == 3),
            "three verticals per two-column row: {out:?}"
        );
        // A full grid rules between every row, with a cross at the column boundary.
        assert!(
            out.iter()
                .any(|l| l.starts_with('├') && l.contains('┼') && l.ends_with('┤')),
            "an inner separator: {out:?}"
        );
        // Bottom border, corners and a bottom junction.
        let last = out.last().expect("some output");
        assert!(
            last.starts_with('└') && last.contains('┴') && last.ends_with('┘'),
            "bottom border: {last:?}"
        );
    }

    #[test]
    fn a_wrapped_row_keeps_its_borders_aligned_down_a_tall_cell() {
        // A description too wide for its column wraps to several lines; the short
        // column is blank on the extra lines, but every content line is the same
        // width and closed by a border in the same place.
        let rows = [
            "| k | description |",
            "|---|---|",
            "| a | one two three four five six seven eight nine ten eleven |",
        ];
        let out = drawn(&rows, 30);
        let body = rows_of(&out);
        assert!(body.len() >= 2, "the tall cell did not wrap: {out:?}");
        let w0 = width::str_width(&body[0]);
        assert!(
            body.iter().all(|l| width::str_width(l) == w0),
            "the wrapped rows are ragged: {out:?}"
        );
        assert!(
            body.iter().all(|l| l.starts_with('│') && l.ends_with('│')),
            "a wrapped line lost a border: {out:?}"
        );
    }

    #[test]
    fn the_box_downgrades_to_ascii_on_a_non_unicode_terminal() {
        // Every box glyph has an ASCII stand-in in `caps::ascii_for`, so the
        // `downgrade` pass turns the whole grid into `+ | -` on a terminal that
        // cannot draw Unicode. Content is ASCII here so only the box is in play.
        let out = drawn(&["| a | b |", "|---|---|", "| 1 | 2 |"], 40);
        for line in &out {
            let ascii = crate::caps::downgrade(line, false);
            assert!(
                ascii.is_ascii(),
                "a box glyph had no ASCII stand-in: {line:?} -> {ascii:?}"
            );
        }
    }

    // ─── splitting ───

    #[test]
    fn a_pipe_inside_inline_code_is_not_a_separator() {
        assert_eq!(cells("| a `x | y` b | c |"), ["a `x | y` b", "c"]);
    }

    #[test]
    fn several_code_spans_each_keep_their_pipes() {
        assert_eq!(cells("| `a|b` | `c|d` |"), ["`a|b`", "`c|d`"]);
    }

    #[test]
    fn a_backslash_escapes_a_pipe_and_the_escape_is_consumed() {
        assert_eq!(cells(r"| a \| b | c |"), ["a | b", "c"]);
    }

    #[test]
    fn a_row_without_edge_delimiters_keeps_its_cells() {
        assert_eq!(cells("a | b"), ["a", "b"]);
    }

    #[test]
    fn an_empty_cell_is_kept_not_collapsed() {
        assert_eq!(cells("| a |  | b |"), ["a", "", "b"]);
    }

    #[test]
    fn cjk_cells_are_kept_whole() {
        assert_eq!(cells("| 中文 | 值 |"), ["中文", "值"]);
    }

    #[test]
    fn a_row_that_does_not_split_into_cells_is_not_a_row() {
        assert_eq!(row("just a sentence"), None);
        assert_eq!(row("---"), None, "a rule is one cell, so not a row");
        assert!(row("a | b").is_some());
    }

    // ─── recognition ───

    #[test]
    fn a_block_without_a_delimiter_row_is_not_a_table() {
        let rows = vec!["Name | Value".to_string(), "foo | bar".to_string()];
        assert!(render(&rows, 40, Style::new()).is_none());
    }

    #[test]
    fn a_lone_delimiter_row_is_not_a_table() {
        let rows = vec!["|---|---|".to_string()];
        assert!(render(&rows, 40, Style::new()).is_none());
    }

    #[test]
    fn a_single_column_table_is_not_read_as_prose() {
        // Every row of a one-column table splits into a single cell, and that
        // used to be the test for "not a row" — the whole table came out as the
        // literal pipes it was written with.
        assert!(row("| 只有一列 |").is_some());
        assert!(row("|:---|").is_some());
        let out = drawn(&["| 只有一列 |", "|:---|", "| 单元格 |"], 40);
        // Boxed like any other: top, header, its separator, body, bottom.
        assert_eq!(out.len(), 5, "top, header, sep, body, bottom: {out:?}");
        assert!(
            !out.iter().any(|l| l.contains('|')),
            "the ASCII pipes leaked into the output: {out:?}"
        );
    }

    #[test]
    fn a_block_of_pipe_lines_without_a_delimiter_is_not_a_table() {
        // The wider row test makes more lines candidates; the delimiter row is
        // still what decides, so a block that never had one stays prose.
        let rows = vec!["see a | b".to_string(), "| just one cell".to_string()];
        assert!(render(&rows, 40, Style::new()).is_none());
    }

    #[test]
    fn a_box_drawing_data_row_converts_to_pipe_form() {
        assert_eq!(row("│ a │ b │").as_deref(), Some("| a | b |"));
    }

    #[test]
    fn a_box_drawing_border_row_becomes_a_delimiter_row() {
        assert_eq!(row("┌───┬───┐").as_deref(), Some("|---|---|"));
        assert_eq!(row("├───┼───┤").as_deref(), Some("|---|---|"));
    }

    #[test]
    fn prose_that_starts_with_a_box_junction_is_not_converted() {
        assert_eq!(row("├ not a border"), None);
    }

    #[test]
    fn a_box_drawing_table_renders_as_columns() {
        // Box rows are converted at detection, not at layout — that is the shape
        // production uses (`mod.rs::table_row` hands `render` the converted row).
        let boxed = ["┌──────┬──────┐", "│ a    │ b    │", "└──────┴──────┘"];
        let rows: Vec<String> = boxed.iter().map(|r| row(r).expect("a box row")).collect();
        let out: Vec<String> = render(&rows, 40, Style::new())
            .expect("a table")
            .iter()
            .map(Line::plain)
            .collect();
        let body = rows_of(&out);
        assert!(
            body[0].contains('a') && body[0].contains('b'),
            "the two columns did not survive the conversion: {out:?}"
        );
        // The ASCII shapes of a pre-drawn table must not leak: it is re-rendered
        // as this table's own box, not passed through.
        assert!(
            !out.iter().any(|l| l.contains('|') || l.contains("---")),
            "the drawn borders leaked into the output: {out:?}"
        );
    }

    // ─── the natural grid ───

    #[test]
    fn a_two_column_table_aligns_every_row() {
        let rows = ["| 名称 | 说明 |", "|---|---|", "| a | bb |", "| ccc | d |"];
        let out = rows_of(&drawn(&rows, 40));
        assert_eq!(out.len(), 3, "one line per row: {out:?}");
        // Every row draws the same cells, whatever the width of what is in them.
        let w = widths(&rows, 40);
        assert!(
            w.windows(2).all(|p| p[0] == p[1]),
            "rows are ragged: {w:?} in {out:?}"
        );
        // In cells, not characters: 名称 is four cells and so is a+c-c.
        assert_eq!(column_at(&out[0], "说明"), column_at(&out[1], "bb"));
        assert_eq!(column_at(&out[1], "bb"), column_at(&out[2], "d"));
    }

    #[test]
    fn the_delimiter_row_is_not_drawn_as_a_row() {
        let out = drawn(&["| a | b |", "|---|---|", "| 1 | 2 |"], 40);
        assert!(
            !out.iter().any(|l| l.contains("---")),
            "the delimiter row leaked into the output: {out:?}"
        );
    }

    #[test]
    fn a_header_cell_is_bold_but_not_highlighted_and_the_body_is_neither() {
        let rows: Vec<String> = ["| a | b |", "|---|---|", "| 1 | 2 |"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Line 0 is the top border; the header is line 1, its separator line 2,
        // the body line 3.
        let base = Style::new();
        let lines = render(&rows, 40, base).unwrap();
        let header = lines[1].spans.iter().find(|s| s.text == "a").unwrap();
        let body = lines[3].spans.iter().find(|s| s.text == "1").unwrap();
        // The header is told from the body by weight alone: bold, but wearing the
        // body's own foreground rather than the accent hue a heading would.
        assert!(header.style.bold, "the header should be bold");
        assert_eq!(header.style.fg, base.fg, "the header must not be highlighted");
        assert!(!body.style.bold, "the body is not bold");
    }

    #[test]
    fn inline_markup_inside_a_cell_is_honoured() {
        let out = rows_of(&drawn(
            &["| a | b |", "|---|---|", "| **bold** | `code` |"],
            40,
        ));
        // The markup is applied, not shown: `bold`/`code`, not `**bold**`/`` `code` ``.
        assert!(
            out[1].contains("bold") && out[1].contains("code"),
            "{out:?}"
        );
        assert!(
            !out[1].contains('*') && !out[1].contains('`'),
            "the markup leaked: {out:?}"
        );
    }

    #[test]
    fn a_pipe_inside_a_cell_is_shown_as_one_cell() {
        let out = rows_of(&drawn(
            &["| expr | note |", "|---|---|", "| `a | b` | a closure |"],
            40,
        ));
        assert!(
            out[1].contains("a | b"),
            "the code span was split across cells: {out:?}"
        );
    }

    // ─── the shape of the grid ───

    /// A full box in BOTH grid tiers: a top border, `├┼┤` rules between every
    /// pair of rows, a bottom border, and every content row closed on both edges.
    #[test]
    fn both_grid_tiers_draw_a_full_box() {
        let parsed: Vec<Vec<String>> = ["| h1 | h2 |", "|---|---|", "| a | b |", "| c | d |"]
            .iter()
            .map(|r| split_row(r))
            .collect();
        let widths = [2usize, 2];
        let tiers = [
            ("natural", aligned(&parsed, &widths, Style::new())),
            ("folded", wrapped_grid(&parsed, &widths, Style::new())),
        ];
        for (tier, lines) in tiers {
            let plain: Vec<String> = lines.iter().map(Line::plain).collect();
            assert!(
                plain[0].starts_with('┌') && plain[0].ends_with('┐'),
                "{tier}: no top border: {plain:?}"
            );
            assert!(
                plain.last().unwrap().starts_with('└') && plain.last().unwrap().ends_with('┘'),
                "{tier}: no bottom border: {plain:?}"
            );
            // A full grid: an inner `├┼┤` rule between the header and the body AND
            // between the two body rows — two of them.
            let inner = plain
                .iter()
                .filter(|l| l.starts_with('├') && l.contains('┼'))
                .count();
            assert_eq!(inner, 2, "{tier}: not a full grid: {plain:?}");
            // Every content row is closed on both edges.
            assert!(
                rows_of(&plain).iter().all(|l| l.ends_with('│')),
                "{tier}: an unclosed row: {plain:?}"
            );
        }
    }

    /// A header with no body is still boxed: a top border, the header row, and a
    /// bottom border — three lines, closed all round.
    #[test]
    fn a_header_only_table_is_still_boxed() {
        let out = drawn(&["| h1 | h2 |", "|---|---|"], 40);
        assert_eq!(out.len(), 3, "top, header, bottom: {out:?}");
        assert!(out[0].starts_with('┌') && out[0].ends_with('┐'), "{out:?}");
        assert!(out[1].starts_with('│') && out[1].ends_with('│'), "{out:?}");
        assert!(out[2].starts_with('└') && out[2].ends_with('┘'), "{out:?}");
    }

    #[test]
    fn an_over_long_delimiter_row_does_not_paint_a_ghost_column() {
        let rows = ["| a | b |", "|---|---|---|", "| 1 | 2 |"];
        let out = rows_of(&drawn(&rows, 40));
        // Two real columns only: three verticals per row, no ghost third column.
        assert!(
            out.iter().all(|l| l.matches('│').count() == 3),
            "a ghost column was painted: {out:?}"
        );
        assert!(out[0].contains('a') && out[0].contains('b'), "{out:?}");
        assert!(out[1].contains('1') && out[1].contains('2'), "{out:?}");
    }

    #[test]
    fn a_stray_trailing_empty_cell_is_trimmed_from_the_grid() {
        let cells = split_row("| a | b | |");
        assert_eq!(cells, ["a", "b", ""]);
        let parsed = vec![cells];
        assert_eq!(effective_ncols(&parsed), 2);
    }

    #[test]
    fn cjk_and_ascii_cells_line_up_to_the_same_column() {
        let rows = ["| 名称 | 说明 |", "|---|---|", "| ab | cd |"];
        let out = rows_of(&drawn(&rows, 40));
        // '说明' begins at the same cell index on the header row as 'cd' does on
        // the body row — the whole point of measuring in cells, not chars.
        let col = |s: &str, mark: &str| {
            #[allow(
                clippy::string_slice,
                reason = "`str::find` returns a byte offset that is always a char boundary"
            )]
            width::str_width(&s[..s.find(mark).unwrap()])
        };
        assert_eq!(col(&out[0], "说明"), col(&out[1], "cd"));
    }

    #[test]
    fn an_emoji_cell_keeps_the_next_column_where_it_belongs() {
        let rows = ["| k | v |", "|---|---|", "| 🙂 | x |", "| name | longer |"];
        let out = rows_of(&drawn(&rows, 40));
        // An emoji is two cells wide: the column after it must not move.
        assert_eq!(column_at(&out[1], "x"), column_at(&out[2], "longer"));
    }

    // ─── fitting, folding, falling back ───

    #[test]
    fn a_table_is_never_wider_than_the_line_it_was_given() {
        let rows: Vec<String> = ["| a | b |", "|---|---|", "| 1 | 2 |"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for w in 1..=40u16 {
            if let Some(lines) = render(&rows, w, Style::new()) {
                for line in &lines {
                    assert!(
                        line.width() <= w as usize,
                        "width {w}: {:?} is {} cells",
                        line.plain(),
                        line.width()
                    );
                }
            }
        }
    }

    #[test]
    fn the_grid_leaves_the_viewport_s_last_column_unused() {
        let rows: Vec<String> = ["| a | b |", "|---|---|", "| 1 | 2 |"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let lines = render(&rows, 30, Style::new()).unwrap();
        let widest = lines.iter().map(Line::width).max().unwrap();
        assert!(
            widest < 30,
            "the grid ran to the edge of a 30-cell line: {widest} cells"
        );
    }

    #[test]
    fn a_somewhat_wide_table_folds_into_a_grid_rather_than_a_list() {
        let rows = [
            "| key | value |",
            "|---|---|",
            "| alpha | a fairly long description here |",
            "| beta | another description of some length |",
        ];
        let out = drawn(&rows, 44);
        assert!(
            out.iter().any(|l| l.contains("key") && l.contains("value")),
            "collapsed to flat although the grid fits when folded: {out:?}"
        );
        // A folded grid still has a box around it, not the flat `key：value` form.
        assert!(
            out[0].starts_with('┌'),
            "the folded tier lost its box: {out:?}"
        );
        assert!(
            out.iter().any(|l| l.contains("alpha")),
            "the body was lost: {out:?}"
        );
    }

    #[test]
    fn a_narrow_terminal_falls_back_to_flat_records() {
        let rows = [
            "| key | value |",
            "|---|---|",
            "| alpha | a fairly long description here |",
        ];
        let out = drawn(&rows, 20);
        assert!(
            out.iter().any(|l| l.contains('：')),
            "no labelled record in the flat fallback: {out:?}"
        );
        assert!(
            out.iter().all(|l| !l.contains('|')),
            "a pipe survived into the fallback: {out:?}"
        );
    }

    /// Two unbreakable identifiers that cannot both fit: the grid's floors are
    /// what makes the flat record the better answer.
    const UNFITTABLE: &[&str] = &[
        "| column_one | column_two |",
        "|---|---|",
        "| a_very_long_identifier | another_long_identifier |",
    ];

    #[test]
    fn flat_records_align_their_labels_into_a_column() {
        let out = drawn(UNFITTABLE, 30);
        let labels: Vec<usize> = out
            .iter()
            .filter(|l| l.contains('：'))
            .map(|l| {
                #[allow(
                    clippy::string_slice,
                    reason = "`str::find` returns a byte offset that is always a char boundary"
                )]
                width::str_width(&l[..l.find('：').unwrap()])
            })
            .collect();
        assert_eq!(labels.len(), 2, "expected a record per row: {out:?}");
        assert_eq!(labels[0], labels[1], "labels are ragged: {out:?}");
    }

    #[test]
    fn flat_records_are_each_labelled_with_their_own_header() {
        let out = drawn(UNFITTABLE, 30);
        assert!(
            out.iter().filter(|l| l.contains('：')).count() == 2,
            "one record per row, each labelled: {out:?}"
        );
        assert!(
            out.iter()
                .filter(|l| l.contains(':'))
                .all(|l| l.contains('：')),
            "the label separator is the full-width colon: {out:?}"
        );
    }

    #[test]
    fn flat_records_separate_rows_with_a_blank_line() {
        let mut rows = UNFITTABLE.to_vec();
        rows.push("| c | d |");
        let out = drawn(&rows, 30);
        assert!(
            out.iter().any(String::is_empty),
            "no blank line between records: {out:?}"
        );
    }

    #[test]
    fn an_over_long_token_char_wraps_instead_of_overflowing() {
        let rows = [
            "| k | v |",
            "|---|---|",
            "| a | averyveryverylongsingletokenhere |",
        ];
        let out = drawn(&rows, 24);
        for line in &out {
            assert!(
                width::str_width(line) <= 24,
                "{line:?} is {} cells",
                width::str_width(line)
            );
        }
    }

    // ─── the width invariant, over every tier ───

    #[test]
    fn nothing_in_a_table_exceeds_the_width_at_any_width() {
        let rows = "| 名称 | 说明 | 备注 |\n|---|---|---|\n\
                    | alpha | a fairly long description of the thing | x |\n\
                    | 🙂 | 中文说明 | `a | b` |\n\
                    | one_very_long_identifier_name | short | 3 |\n\
                    ┌───┬───┐\n│ a │ b │\n└───┴───┘";
        let rows: Vec<String> = rows.split('\n').map(|s| s.to_string()).collect();
        for w in 1..=100u16 {
            if let Some(lines) = render(&rows, w, Style::new()) {
                for line in &lines {
                    assert!(
                        line.width() <= w as usize,
                        "width {w}: {:?} is {} cells",
                        line.plain(),
                        line.width()
                    );
                }
            }
        }
    }

    // ─── word_wrap ───

    #[test]
    fn word_wrap_breaks_at_spaces_and_folds_a_long_token() {
        assert_eq!(word_wrap("a b c", 3), ["a b", "c"]);
        assert_eq!(word_wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(word_wrap("", 4), [""]);
        // A wide character is never halved.
        assert_eq!(word_wrap("中中", 3), ["中", "中"]);
    }

    #[test]
    fn a_token_exactly_the_column_width_is_not_fragmented() {
        assert_eq!(word_wrap("abcd", 4), ["abcd"]);
        assert_eq!(word_wrap("abcd ef", 4), ["abcd", "ef"]);
    }
}
