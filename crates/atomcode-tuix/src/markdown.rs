// crates/atomcode-tuix/src/markdown.rs
//
// Very small inline-markdown renderer. Operates on single, complete lines —
// not a full CommonMark parser. Handles:
//   **bold**   → SGR bold
//   *italic*   → SGR italic
//   `code`     → SGR fg colour (code-ish yellow/cyan)
//
// Rendered output is ANSI bytes, safe to `write_all` in raw mode (no bare
// \n). Callers are expected to split the model's output on '\n' and call
// `render_inline_line` per complete line.

use crate::terminal::TerminalCaps;

/// Render one complete line of assistant text as ANSI-styled bytes.
/// Does NOT emit a trailing newline; caller appends `\r\n` if needed.
pub fn render_inline_line(line: &str, caps: TerminalCaps) -> String {
    if !caps.colors {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + 16);
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    // Potential **bold**. Look for closing **.
                    chars.next(); // consume second *
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
                                // lone *, keep as literal
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
                    // Potential *italic*. Look for closing *.
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
                // Inline `code` — look for matching backtick.
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
                    // Yellowish code colour (SGR 38;2;R;G;B for truecolor).
                    out.push_str("\x1b[38;2;205;170;90m");
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
    fn no_color_passes_through() {
        assert_eq!(render_inline_line("**bold**", plain_caps()), "**bold**");
    }

    #[test]
    fn bold_wraps_with_sgr() {
        let out = render_inline_line("**bold**", caps());
        assert_eq!(out, "\x1b[1mbold\x1b[22m");
    }

    #[test]
    fn italic_wraps_with_sgr() {
        let out = render_inline_line("*em*", caps());
        assert_eq!(out, "\x1b[3mem\x1b[23m");
    }

    #[test]
    fn inline_code_coloured() {
        let out = render_inline_line("run `cargo test`", caps());
        assert!(out.contains("\x1b[38;2;205;170;90mcargo test\x1b[39m"));
        assert!(out.starts_with("run "));
    }

    #[test]
    fn unclosed_bold_literal() {
        let out = render_inline_line("**hello", caps());
        assert_eq!(out, "**hello");
    }

    #[test]
    fn unclosed_code_literal() {
        let out = render_inline_line("`hello", caps());
        assert_eq!(out, "`hello");
    }

    #[test]
    fn mixed_content() {
        let out = render_inline_line("see `code` and **bold**", caps());
        assert!(out.contains("\x1b[38;2;205;170;90mcode\x1b[39m"));
        assert!(out.contains("\x1b[1mbold\x1b[22m"));
    }

    #[test]
    fn cjk_inside_bold() {
        let out = render_inline_line("**你好**", caps());
        assert_eq!(out, "\x1b[1m你好\x1b[22m");
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(render_inline_line("hello world", caps()), "hello world");
    }
}
