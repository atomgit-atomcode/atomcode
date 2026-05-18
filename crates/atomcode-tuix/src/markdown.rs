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
    /// Lines accumulated between an opening and closing code fence.
    /// Flushed through `highlight::highlight_block` on close fence so
    /// the syntax highlighter sees the whole block at once. Code thus
    /// appears in one chunk at fence close rather than streaming
    /// line-by-line.
    pub code_buf: Vec<String>,
    /// Language tag captured from the opening fence (`"rust"` from
    /// ```` ```rust ````). `None` for fences with no tag.
    pub code_lang: Option<String>,
}

impl MdState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn reset(&mut self) {
        self.in_code_block = false;
        self.table_buf.clear();
        self.code_buf.clear();
        self.code_lang = None;
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
    // OPEN fence: capture the language tag (e.g., `rust` in ```rust),
    // start buffering body lines into `state.code_buf`. We don't emit
    // anything for the body until close fence — the syntax highlighter
    // needs the whole block at once to classify multi-line strings /
    // block comments correctly.
    //
    // CLOSE fence: flush the buffered block through `highlight::highlight_block`,
    // which handles caps gating (no-color path returns plain 2-space-indented
    // text, matching the pre-existing CC-style behavior).
    if is_fence(trimmed) {
        if state.in_code_block {
            // CLOSE
            let source = state.code_buf.join("\n");
            let highlighted = crate::highlight::highlight_block(
                state.code_lang.as_deref(),
                &source,
                caps,
            );
            state.in_code_block = false;
            state.code_buf.clear();
            state.code_lang = None;
            return Some(prepend(highlighted));
        } else {
            // OPEN — extract optional language tag.
            state.in_code_block = true;
            state.code_lang = parse_fence_lang(trimmed);
            state.code_buf.clear();
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
                        '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘' => '|',
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
                    // Bold only (no fg colour). Earlier iterations used
                    // `\x1b[1;97m` (bright white) and then truecolor
                    // blue-500 (`\x1b[1;38;2;59;130;246m`) to dodge
                    // terminal palette remap. In long mixed output
                    // (markdown headings + code fences + many backtick
                    // spans) the cumulative colour load competed with
                    // code blocks for the eye's anchor — every
                    // `path/to/foo.rs` shouted as loud as a 30-line
                    // code fence. Bold alone keeps inline code
                    // distinguishable from prose without painting half
                    // the screen.
                    out.push_str("\x1b[1m");
                    out.push_str(&inner);
                    out.push_str("\x1b[22m");
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

/// Extract the language tag from an opening fence line. Handles both
/// backtick and tilde fences; returns `None` if no tag is present or
/// the line is just the fence character.
///
/// Examples:
///   "```rust"        -> Some("rust")
///   "```rust  "      -> Some("rust")
///   "```Rust"        -> Some("rust")     ← lowercased
///   "```"            -> None
///   "~~~python"      -> Some("python")
fn parse_fence_lang(trimmed: &str) -> Option<String> {
    let after = trimmed
        .trim_start_matches('`')
        .trim_start_matches('~')
        .trim();
    if after.is_empty() {
        None
    } else {
        Some(after.to_lowercase())
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
        // Inline code uses bold ONLY (no fg colour). Earlier
        // iterations tried bright-white and truecolor blue-500;
        // both painted too many backtick spans on screen. Bold alone
        // keeps inline code distinguishable from prose without
        // competing with code-block emphasis.
        let rendered = render_inline_line("`x`", caps());
        assert!(
            rendered.contains("\x1b[1mx"),
            "inline code must open bold (SGR 1) without fg colour: {}",
            rendered
        );
        assert!(
            !rendered.contains("\x1b[1;97m"),
            "inline code must NOT include bright-white SGR 97: {}",
            rendered
        );
        assert!(
            !rendered.contains("\x1b[1;38;2;"),
            "inline code must NOT include truecolor RGB anymore: {}",
            rendered
        );
    }

    #[test]
    fn fenced_code_block_renders_as_plain_indented_code() {
        // CC-style: code blocks are plain text with a 2-space
        // left margin and default foreground colour. No `│` gutter
        // (turns to mojibake on Windows cmd.exe under non-UTF-8
        // codepage), no bold+bright white, no truecolor blue. Pin
        // the shape so a future "let's add a fancy bar" refactor
        // catches itself in CI.
        let mut state = MdState::new();
        let _ = render_line("```", &mut state, caps()); // open fence
        let inside = render_line("let x = 1;", &mut state, caps()).unwrap_or_default();
        assert!(
            inside.contains("  let x = 1;"),
            "fenced code body should appear with 2-space indent: {:?}",
            inside
        );
        assert!(
            !inside.contains('│'),
            "fenced code block must NOT emit `│` left bar (Windows cmd compat): {:?}",
            inside
        );
        assert!(
            !inside.contains("\x1b[1;97m"),
            "fenced code block must NOT bold+bright-white the content: {:?}",
            inside
        );
        assert!(
            !inside.contains("\x1b[1;38;2;"),
            "fenced code block must NOT truecolor-blue the content: {:?}",
            inside
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

    /// Wide terminal: a box-drawing table re-renders as a clean box at
    /// natural widths (the input is converted to pipe form, then
    /// `flush_aligned_table_with_width` re-emits its own box drawing).
    #[test]
    fn box_drawing_table_re_renders_as_box_when_fits() {
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
        assert!(out.contains('┌'), "wide terminal should keep box rendering:\n{out}");
        assert!(out.contains('└'));
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
    fn mdstate_default_has_empty_code_buf_and_no_lang() {
        let s = MdState::new();
        assert!(s.code_buf.is_empty(), "code_buf must start empty");
        assert!(s.code_lang.is_none(), "code_lang must start None");
    }

    #[test]
    fn mdstate_reset_clears_code_buf_and_lang() {
        let mut s = MdState::new();
        s.code_buf.push("dirty".into());
        s.code_lang = Some("rust".into());
        s.in_code_block = true;
        s.reset();
        assert!(s.code_buf.is_empty(), "reset must clear code_buf");
        assert!(s.code_lang.is_none(), "reset must clear code_lang");
        assert!(!s.in_code_block, "reset must clear in_code_block");
    }

    #[test]
    fn fence_open_with_lang_captures_lang_and_buffers_lines() {
        let mut st = MdState::new();
        // Open fence with `rust` tag — language captured, no body output yet.
        assert!(render_line("```rust", &mut st, caps()).is_none());
        assert_eq!(st.code_lang.as_deref(), Some("rust"));
        assert!(st.in_code_block);

        // Body lines accumulate to code_buf, no output emitted yet.
        assert!(render_line("let x = 1;", &mut st, caps()).is_none());
        assert!(render_line("let y = 2;", &mut st, caps()).is_none());
        assert_eq!(st.code_buf.len(), 2);
    }

    #[test]
    fn fence_close_flushes_buffered_block_as_one_chunk() {
        // Use plain_caps so the substring checks see the literal source text
        // — with truecolor caps, syntect interleaves ANSI escapes between
        // every token boundary (keywords/identifiers/operators each get
        // their own SGR pair), so `out.contains("let x = 1;")` won't match.
        // The colored path is covered separately by
        // `fence_close_with_colors_produces_truecolor_ansi`.
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
        assert!(st.code_lang.is_none());
    }

    #[test]
    fn fence_close_with_colors_produces_truecolor_ansi() {
        let mut st = MdState::new();
        render_line("```rust", &mut st, caps());
        render_line("fn main() {}", &mut st, caps());
        let out = render_line("```", &mut st, caps()).unwrap();
        assert!(
            out.contains("\x1b[38;2;"),
            "tinted output must contain a truecolor SGR, got: {:?}",
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
        assert!(!out.contains('\x1b'), "plain_caps must emit zero ANSI, got: {:?}", out);
    }

    #[test]
    fn fence_open_with_no_lang_tag_buffers_with_none_lang() {
        let mut st = MdState::new();
        assert!(render_line("```", &mut st, caps()).is_none());
        assert_eq!(st.code_lang, None);
        assert!(st.in_code_block);
    }

    #[test]
    fn lang_tag_with_trailing_whitespace_is_trimmed() {
        let mut st = MdState::new();
        render_line("```rust  ", &mut st, caps());
        assert_eq!(st.code_lang.as_deref(), Some("rust"));
    }
}
