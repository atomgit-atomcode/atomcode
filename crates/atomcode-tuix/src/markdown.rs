// crates/atomcode-tuix/src/markdown.rs
//
// Line-oriented markdown renderer. Handles:
//   **bold** / *italic* / `code` (inline)
//   # / ## / ### headings
//   - / * bullet lists
//   ```fenced code blocks``` (state-tracked)
//   --- horizontal rules
// Tables are passed through as raw text (pipes show literally).

use crate::terminal::TerminalCaps;

/// Parser state maintained across lines of a streamed response.
#[derive(Default)]
pub struct MdState {
    pub in_code_block: bool,
    /// Accumulates consecutive `|…|` rows; flushed as an aligned block
    /// when a non-table line arrives.
    pub table_buf: Vec<String>,
}

impl MdState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn reset(&mut self) {
        self.in_code_block = false;
        self.table_buf.clear();
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

    // Table row: buffer and defer emit until block ends.
    if !state.in_code_block && trimmed.starts_with('|') {
        state.table_buf.push(trimmed.to_string());
        return None;
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

    // Fenced code block fence (``` or ~~~)
    if is_fence(trimmed) {
        state.in_code_block = !state.in_code_block;
        return prefix_only();
    }

    // Inside code block: render in truecolor blue-500 (#3B82F6, RGB
    // 59,130,246) + bold. Direct RGB sidesteps the bright-XX palette
    // remap problem — `\x1b[1;97m` (bright white) was invisible on
    // iTerm2 light preset, `\x1b[1;96m` (bright cyan) was a washed-out
    // teal there. blue-500 has lightness ≈ 0.6 so it reads with at
    // least 4:1 contrast against pure white AND pure black backgrounds.
    if state.in_code_block {
        let body = if caps.colors {
            format!("\x1b[1;38;2;59;130;246m{}\x1b[22;39m", line)
        } else {
            line.to_string()
        };
        return Some(prepend(body));
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
        let inner = render_inline(rest, caps);
        let body = if !caps.colors {
            format!("{} {}", "#".repeat(level as usize), inner)
        } else {
            match level {
                1 | 2 | 3 => format!("\x1b[1;96m{}\x1b[22;39m", inner),
                _ => format!("\x1b[3m{}\x1b[23m", inner),
            }
        };
        return Some(prepend(body));
    }

    // Unordered list: `- text` / `* text`
    if let Some((indent, rest)) = parse_list_item(line) {
        let inner = render_inline(rest, caps);
        return Some(prepend(format!("{}• {}", " ".repeat(indent), inner)));
    }

    // Default: inline-only
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
    if state.table_buf.is_empty() {
        return None;
    }
    let t = flush_aligned_table_with_width(&state.table_buf, caps, max_width);
    state.table_buf.clear();
    Some(t)
}

/// Flush a buffered markdown table as a column-aligned block. Computes the
/// max display width per column, pads every cell accordingly, renders with
/// `│`/`┼`/`─` box chars in muted gray. Inline markdown inside cells is
/// honoured.
pub fn flush_aligned_table(rows: &[String], caps: TerminalCaps) -> String {
    flush_aligned_table_with_width(rows, caps, 0)
}

/// Width-aware variant. When `max_width > 0` and the table can't fit at its
/// natural column widths, fall back to a flat key/value record format
/// (`header: cell` per line, blank line between rows) so no information is
/// lost to per-cell truncation. `max_width = 0` keeps box-table rendering
/// at natural widths regardless of size.
pub fn flush_aligned_table_with_width(
    rows: &[String],
    caps: TerminalCaps,
    max_width: usize,
) -> String {
    // Parse each row: strip leading/trailing '|', split by '|', trim cells.
    let parsed: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let s = r.trim_start_matches('|').trim_end_matches('|');
            s.split('|').map(|c| c.trim().to_string()).collect()
        })
        .collect();

    // Identify separator row(s) — cells match `[-: ]+` only.
    let is_sep = |row: &[String]| -> bool {
        row.iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
    };

    let ncols = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncols == 0 {
        return String::new();
    }

    // Compute natural column widths from non-separator rows. We do NOT cap
    // these — the cap-and-truncate-with-… approach the previous code took
    // chopped real content out of cells and made wide tables in narrow
    // terminals unreadable. Instead, if the natural table doesn't fit, the
    // flat-mode fallback below renders every cell in full.
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

    // Total width of one rendered row at natural widths:
    //   `│` + per-col ` cell ` + `│` between/after each col
    //   = 1 + sum(w + 3 for w in col_widths)
    // If this exceeds the terminal budget, switch to flat mode.
    let natural_row_width: usize = 1 + col_widths.iter().map(|w| w + 3).sum::<usize>();
    if max_width > 0 && natural_row_width > max_width {
        return render_flat_table(&parsed, caps);
    }

    // Bright-black / DarkGrey (SGR 90) — table borders are chrome,
    // not content. Cyan (SGR 96) made them collide with the input
    // box separator and the inline-code colour, collapsing the
    // visual hierarchy. Gray reads as quiet structure and lets
    // header text + cell content carry the visual weight.
    let border_on = if caps.colors { "\x1b[90m" } else { "" };
    let border_off = if caps.colors { "\x1b[39m" } else { "" };

    // Draw a horizontal rule row with given connector characters.
    let rule = |left: char, mid: char, right: char| -> String {
        let mut s = String::new();
        s.push_str(border_on);
        s.push(left);
        for (j, w) in col_widths.iter().enumerate() {
            for _ in 0..(w + 2) {
                s.push('─');
            }
            if j + 1 < col_widths.len() {
                s.push(mid);
            }
        }
        s.push(right);
        s.push_str(border_off);
        s
    };

    let data_rows: Vec<&Vec<String>> = parsed.iter().filter(|r| !is_sep(r)).collect();

    let mut out = String::new();
    // Top border: ┌─┬─┐
    out.push_str(&rule('┌', '┬', '┐'));
    out.push('\n');

    for (i, row) in data_rows.iter().enumerate() {
        // Data row: │ cell │ cell │
        out.push_str(border_on);
        out.push('│');
        out.push_str(border_off);
        for (j, w) in col_widths.iter().enumerate() {
            let cell = row.get(j).map(|s| s.as_str()).unwrap_or("");
            let plain_w = crate::width::display_width(&strip_md_for_width(cell));
            let body = render_inline(cell, caps);
            out.push(' ');
            out.push_str(&body);
            let pad = w.saturating_sub(plain_w);
            for _ in 0..pad {
                out.push(' ');
            }
            out.push(' ');
            out.push_str(border_on);
            out.push('│');
            out.push_str(border_off);
        }
        out.push('\n');

        // Separator between every pair of rows: ├─┼─┤
        if i + 1 < data_rows.len() {
            out.push_str(&rule('├', '┼', '┤'));
            out.push('\n');
        }
    }

    // Bottom border: └─┴─┘
    out.push_str(&rule('└', '┴', '┘'));
    out
}

/// Narrow-terminal fallback for tables that can't fit at natural column
/// widths. Each data row is expanded into N lines of `header：cell` (one
/// per column), with a blank line between successive rows. Soft-wrapping
/// of long lines is left to the caller's downstream wrap stage so the
/// terminal width budget is honoured without losing any cell content.
fn render_flat_table(parsed: &[Vec<String>], caps: TerminalCaps) -> String {
    let is_sep = |row: &[String]| -> bool {
        row.iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
    };
    let has_sep = parsed.iter().any(|r| is_sep(r));
    let mut data_iter = parsed.iter().filter(|r| !is_sep(r));

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

    let ncols = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
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
                out.push_str(&h_rendered);
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

fn strip_md_for_width(s: &str) -> String {
    // Remove markdown markers that add bytes but no display width.
    s.replace("**", "").replace('`', "")
}

/// Legacy single-line inline renderer — kept for direct callers (tests,
/// simple assistant lines). Does not track block state.
pub fn render_inline_line(line: &str, caps: TerminalCaps) -> String {
    render_inline(line, caps)
}

// ─── Helpers ───

fn render_inline(line: &str, caps: TerminalCaps) -> String {
    if !caps.colors {
        return line.to_string();
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
                        out.push_str("\x1b[1m");
                        out.push_str(&inner);
                        out.push_str("\x1b[22m");
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
                        out.push_str("\x1b[3m");
                        out.push_str(&inner);
                        out.push_str("\x1b[23m");
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
                    // Bold + truecolor blue-500 (#3B82F6). Same rationale
                    // as the code-block path above — direct RGB so the
                    // colour survives terminal palette remap and stays
                    // readable on both light and dark backgrounds.
                    out.push_str("\x1b[1;38;2;59;130;246m");
                    out.push_str(&inner);
                    out.push_str("\x1b[22;39m");
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

fn is_fence(trimmed: &str) -> bool {
    let mut chars = trimmed.chars();
    match chars.next() {
        Some('`') => {
            trimmed.len() >= 3 && trimmed.as_bytes()[1] == b'`' && trimmed.as_bytes()[2] == b'`'
        }
        Some('~') => {
            trimmed.len() >= 3 && trimmed.as_bytes()[1] == b'~' && trimmed.as_bytes()[2] == b'~'
        }
        _ => false,
    }
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

fn parse_list_item(line: &str) -> Option<(usize, &str)> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    let rest = &line[indent..];
    if let Some(r) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
        Some((indent, r))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn inline_bold() {
        assert_eq!(
            render_inline_line("**bold**", caps()),
            "\x1b[1mbold\x1b[22m"
        );
    }

    #[test]
    fn inline_italic() {
        assert_eq!(render_inline_line("*em*", caps()), "\x1b[3mem\x1b[23m");
    }

    #[test]
    fn inline_code() {
        // Inline code uses bold + truecolor blue-500 (#3B82F6, RGB
        // 59,130,246). Truecolor sidesteps the bright-XX palette remap
        // problem so the colour stays readable on iTerm2 light preset
        // (where bright-white was invisible and bright-cyan was a
        // washed-out pastel teal).
        assert!(
            render_inline_line("`x`", caps()).contains("\x1b[1;38;2;59;130;246mx"),
            "got: {:?}",
            render_inline_line("`x`", caps())
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
        // H1-H3 use bold + bright cyan (`\x1b[1;96m`) so headings sit
        // on a separate colour layer from default-colour body text.
        assert!(out.contains("\x1b[1;96m"), "H2 should be bold + bright cyan, got: {:?}", out);
    }

    #[test]
    fn heading_h4_uses_italic_not_color() {
        let mut st = MdState::new();
        let out = render_line("#### Sub-deep", &mut st, caps()).unwrap();
        assert!(out.contains("Sub-deep"));
        // H4+ keeps italic-only — distinct from coloured H1-H3 without
        // adding a third colour tier.
        assert!(out.contains("\x1b[3m"), "H4 should be italic, got: {:?}", out);
        assert!(!out.contains("\x1b[1;96m"), "H4 must not pick up the H1-H3 cyan");
    }

    #[test]
    fn heading_plain_keeps_hashes() {
        let mut st = MdState::new();
        let out = render_line("### Sub", &mut st, plain_caps()).unwrap();
        assert_eq!(out, "### Sub");
    }

    #[test]
    fn fence_toggles_state_and_hides() {
        let mut st = MdState::new();
        assert!(render_line("```rust", &mut st, caps()).is_none());
        assert!(st.in_code_block);
        let inside = render_line("let x = 1;", &mut st, caps()).unwrap();
        assert!(inside.contains("let x = 1;"));
        // Inside code block, inline markdown is NOT parsed
        let inside2 = render_line("**not bold**", &mut st, caps()).unwrap();
        assert!(inside2.contains("**not bold**"));
        assert!(render_line("```", &mut st, caps()).is_none());
        assert!(!st.in_code_block);
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
        let mut st = MdState::new();
        let out = render_line("- item", &mut st, caps()).unwrap();
        assert!(out.starts_with("• "));
    }

    #[test]
    fn list_nested_indent() {
        let mut st = MdState::new();
        let out = render_line("  - nested", &mut st, caps()).unwrap();
        assert!(out.starts_with("  • "));
    }

    #[test]
    fn cjk_bold() {
        assert_eq!(
            render_inline_line("**你好**", caps()),
            "\x1b[1m你好\x1b[22m"
        );
    }

    /// Wide-enough terminal: render as a normal box-drawing table at the
    /// table's natural column widths. No truncation, no ellipsis.
    #[test]
    fn wide_table_renders_as_box_at_natural_widths() {
        let rows = vec![
            "| Feature | Status |".to_string(),
            "|---------|--------|".to_string(),
            "| login   | done   |".to_string(),
            "| signup  | wip    |".to_string(),
        ];
        // Plenty of room — natural width is well under 80.
        let out = flush_aligned_table_with_width(&rows, plain_caps(), 80);
        assert!(out.contains('┌'));
        assert!(out.contains('│'));
        assert!(out.contains('└'));
        // Cell contents survive in full.
        assert!(out.contains("login"));
        assert!(out.contains("signup"));
        // No ellipsis introduced.
        assert!(!out.contains('…'));
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
        assert!(!out.contains('│'), "narrow output must not contain border │");
        assert!(!out.contains('┌'), "narrow output must not contain top corner");

        // Every cell value survives in full — no truncation.
        assert!(out.contains("AtomCode Air"));
        assert!(out.contains("Python+"));

        // Each header label appears once per data row.
        let count_neng_li = out.matches("能力").count();
        assert_eq!(count_neng_li, 2, "header `能力` should label both data rows");
        let count_cursor = out.matches("Cursor").count();
        assert_eq!(count_cursor, 2, "header `Cursor` should label both data rows");

        // Records are separated by a blank line.
        assert!(
            out.contains("\n\n"),
            "expected blank line between flat records"
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
        assert!(wide.contains('│'), "80 cols should render as box");

        let narrow = flush_aligned_table_with_width(&rows, plain_caps(), 20);
        assert!(!narrow.contains('│'), "20 cols should fall back to flat");
    }
}
