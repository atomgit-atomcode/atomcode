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
    let trimmed = line.trim();

    // Table row: buffer and defer emit until block ends.
    if !state.in_code_block && trimmed.starts_with('|') {
        state.table_buf.push(trimmed.to_string());
        return None;
    }

    // Non-table line arriving after buffered rows: flush as aligned block.
    let prefix = if !state.table_buf.is_empty() {
        let t = flush_aligned_table(&state.table_buf, caps);
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

    // Inside code block: render in bright cyan (SGR 96) with no inline
    // parsing. Bright cyan is the conventional "code" tone across themes
    // and reads vividly on both light and dark backgrounds.
    if state.in_code_block {
        let body = if caps.colors {
            format!("\x1b[96m{}\x1b[39m", line)
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

    // Heading — express hierarchy with SGR weight (bold) rather than
    // specific colours. Top levels stay in the terminal's default
    // foreground (always readable); lower levels drop to muted grey.
    if let Some((level, rest)) = parse_heading(line) {
        let inner = render_inline(rest, caps);
        let body = if !caps.colors {
            format!("{} {}", "#".repeat(level as usize), inner)
        } else {
            match level {
                1 | 2 => format!("\x1b[1m{}\x1b[22m", inner),
                3 => format!("\x1b[1m{}\x1b[22m", inner),
                _ => format!("\x1b[90m{}\x1b[39m", inner),
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
    if state.table_buf.is_empty() {
        return None;
    }
    let t = flush_aligned_table(&state.table_buf, caps);
    state.table_buf.clear();
    Some(t)
}

/// Flush a buffered markdown table as a column-aligned block. Computes the
/// max display width per column, pads every cell accordingly, renders with
/// `│`/`┼`/`─` box chars in muted gray. Inline markdown inside cells is
/// honoured.
pub fn flush_aligned_table(rows: &[String], caps: TerminalCaps) -> String {
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

    // Compute col widths from non-separator rows, using display width of
    // the plaintext (markdown markers stripped for width only).
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
            let rendered = render_inline(cell, caps);
            out.push(' ');
            out.push_str(&rendered);
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
                    out.push_str("\x1b[96m"); // bright cyan (SGR 96) — inline code
                    out.push_str(&inner);
                    out.push_str("\x1b[39m");
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
            no_color: false,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
        })
    }
    fn plain_caps() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: true,
            term: Some("xterm".to_string()),
            colorterm: None,
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
        // Inline code uses SGR 96 (bright cyan) — reads vividly on both
        // light and dark terminal themes.
        assert!(render_inline_line("`x`", caps()).contains("\x1b[96mx"));
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
        // Headings now use SGR bold (\x1b[1m) with default foreground —
        // readable on both light and dark terminal themes.
        assert!(out.contains("\x1b[1m"));
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
}
