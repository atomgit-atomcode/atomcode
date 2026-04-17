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
}

impl MdState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn reset(&mut self) {
        self.in_code_block = false;
    }
}

/// Render one complete line with block- and inline-level markdown applied.
/// Returns None if the line should be omitted from output (e.g., a fence
/// marker ``` that toggles code-block state but isn't itself visible text).
pub fn render_line(line: &str, state: &mut MdState, caps: TerminalCaps) -> Option<String> {
    let trimmed = line.trim();

    // Fenced code block fence (``` or ~~~)
    if is_fence(trimmed) {
        state.in_code_block = !state.in_code_block;
        return None;
    }

    // Inside code block: render in a soft teal with no inline parsing
    if state.in_code_block {
        if caps.colors {
            return Some(format!("\x1b[38;2;175;205;190m{}\x1b[39m", line));
        }
        return Some(line.to_string());
    }

    // Horizontal rule — thin bright gray line
    if is_hrule(trimmed) {
        let rule = "─".repeat(60);
        if caps.colors {
            return Some(format!("\x1b[38;2;130;130;140m{}\x1b[39m", rule));
        }
        return Some(rule);
    }

    // Table row (starts with `|`): replace pipes with box chars, thin bright line.
    if trimmed.starts_with('|') {
        return Some(render_table_line(trimmed, caps));
    }

    // Heading — no bold, pure colour weight by level.
    if let Some((level, rest)) = parse_heading(line) {
        let inner = render_inline(rest, caps);
        if !caps.colors {
            return Some(format!("{} {}", "#".repeat(level as usize), inner));
        }
        return Some(match level {
            1 => format!("\x1b[38;2;205;175;215m{}\x1b[39m", inner), // brand lavender
            2 => format!("\x1b[38;2;170;170;180m{}\x1b[39m", inner), // secondary gray
            _ => format!("\x1b[38;2;130;130;140m{}\x1b[39m", inner), // muted border gray
        });
    }

    // Unordered list: `- text` / `* text`
    if let Some((indent, rest)) = parse_list_item(line) {
        let inner = render_inline(rest, caps);
        return Some(format!("{}• {}", " ".repeat(indent), inner));
    }

    // Default: inline-only
    Some(render_inline(line, caps))
}

/// Render a markdown table line. Converts `|` to `│` and `-` to `─`,
/// separators get `┼` at junctions. Colour: thin bright gray border.
fn render_table_line(line: &str, caps: TerminalCaps) -> String {
    let is_separator = line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '));
    let converted: String = if is_separator {
        line.chars()
            .map(|c| match c {
                '|' => '┼',
                '-' => '─',
                other => other,
            })
            .collect()
    } else {
        line.chars()
            .map(|c| match c {
                '|' => '│',
                other => other,
            })
            .collect()
    };
    if !caps.colors {
        return converted;
    }
    // Emit with the border colour applied to borders only. For simplicity,
    // colour the entire line with the border tone — content reads fine on
    // the thin gray tint.
    format!("\x1b[38;2;130;130;140m{}\x1b[39m", converted)
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
                    out.push_str("\x1b[38;2;175;205;190m"); // soft teal
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
            trimmed.len() >= 3
                && trimmed.as_bytes()[1] == b'`'
                && trimmed.as_bytes()[2] == b'`'
        }
        Some('~') => {
            trimmed.len() >= 3
                && trimmed.as_bytes()[1] == b'~'
                && trimmed.as_bytes()[2] == b'~'
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
        assert_eq!(render_inline_line("**bold**", caps()), "\x1b[1mbold\x1b[22m");
    }

    #[test]
    fn inline_italic() {
        assert_eq!(render_inline_line("*em*", caps()), "\x1b[3mem\x1b[23m");
    }

    #[test]
    fn inline_code() {
        assert!(render_inline_line("`x`", caps()).contains("\x1b[38;2;175;205;190mx"));
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
        // Headings now use colour-only (no bold), so SGR starts with 38.
        assert!(out.contains("\x1b[38;2;"));
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
    fn hrule_becomes_box_chars() {
        let mut st = MdState::new();
        let out = render_line("---", &mut st, caps()).unwrap();
        assert!(out.contains("─"));
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
        assert_eq!(render_inline_line("**你好**", caps()), "\x1b[1m你好\x1b[22m");
    }
}
