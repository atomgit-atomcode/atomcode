// crates/atomcode-tuix/src/highlight/mod.rs
//
// Public entry for fenced-code-block syntax highlight. Dispatch:
//
//   caps.colors == false             -> plain indented passthrough (no ANSI)
//   lang_hint == None                -> plain indented passthrough
//   syntect doesn't know the lang    -> plain indented passthrough
//   syntect highlights successfully  -> tinted ANSI with 2-space left indent
//
// Output is a multi-line string where every line is prefixed with 2 spaces
// (matches the pre-existing CC-style code-block indent). Caller (`markdown.rs`)
// inserts it verbatim into the body stream.

use crate::terminal::TerminalCaps;

pub mod theme;

/// Highlight a complete fenced code block and return the indented, ANSI-tinted
/// multi-line string ready for `push_markdown_body`.
pub fn highlight_block(
    lang_hint: Option<&str>,
    source: &str,
    caps: TerminalCaps,
) -> String {
    if !caps.colors {
        return indent_plain(source);
    }
    if let Some(lang) = lang_hint {
        if let Some(tinted) = highlight_with_syntect(source, lang) {
            return indent_lines(&tinted);
        }
    }
    indent_plain(source)
}

/// syntect path. Stubbed in Task 3; filled in by Task 4. Returning `None`
/// here makes the caller fall through to plain-indent passthrough, which
/// is the correct degraded behavior — exercise the dispatch in tests now.
fn highlight_with_syntect(_source: &str, _lang: &str) -> Option<String> {
    None
}

/// Per-line "  " indent for the no-color / unknown-lang path (matches
/// pre-existing `format!("  {}", line)` behavior in `markdown.rs`).
fn indent_plain(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 32);
    let mut first = true;
    for line in source.split('\n') {
        if !first {
            out.push('\n');
        }
        out.push_str("  ");
        out.push_str(line);
        first = false;
    }
    out
}

/// Per-line "  " indent for tinted output. ANSI escapes ride along inside
/// each line — terminals don't count escape bytes as columns.
fn indent_lines(tinted: &str) -> String {
    let mut out = String::with_capacity(tinted.len() + 32);
    let mut first = true;
    for line in tinted.split('\n') {
        if !first {
            out.push('\n');
        }
        out.push_str("  ");
        out.push_str(line);
        first = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{EnvView, TerminalCaps};

    fn caps_color() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
            lang: Some("en_US.UTF-8".to_string()),
            ..Default::default()
        })
    }

    fn caps_nocolor() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: true,
            term: Some("xterm".to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn no_color_bypasses_highlight_returns_plain_indented() {
        let out = highlight_block(Some("rust"), "let x = 1;", caps_nocolor());
        assert_eq!(out, "  let x = 1;");
        assert!(!out.contains('\x1b'), "no_color path must emit zero ANSI");
    }

    #[test]
    fn no_color_multiline_each_line_indented() {
        let out = highlight_block(Some("rust"), "let x = 1;\nlet y = 2;", caps_nocolor());
        assert_eq!(out, "  let x = 1;\n  let y = 2;");
    }

    #[test]
    fn missing_lang_tag_falls_back_to_plain_indent() {
        let out = highlight_block(None, "x = 42", caps_color());
        assert_eq!(out, "  x = 42");
        assert!(!out.contains('\x1b'), "no lang tag means no ANSI for now");
    }

    #[test]
    fn unknown_lang_via_stub_falls_back_to_plain_indent() {
        // Task 3 stubs highlight_with_syntect to None; dispatch lands in
        // plain-indent for every lang. Task 4 upgrades the rust case below
        // to assert keyword highlighting; this test stays as the proof
        // that unknown / unrecognized langs always degrade safely.
        let out = highlight_block(Some("frobnicate"), "x = 42", caps_color());
        assert_eq!(out, "  x = 42");
    }

    #[test]
    fn supported_lang_currently_falls_through_to_plain_via_stub() {
        // Same input via "rust" — currently stubbed, will be upgraded in Task 4.
        let out = highlight_block(Some("rust"), "fn main() {}", caps_color());
        assert_eq!(out, "  fn main() {}");
    }

    #[test]
    fn empty_source_returns_indent_only() {
        let out = highlight_block(None, "", caps_nocolor());
        assert_eq!(out, "  ");
    }

    #[test]
    fn trailing_newline_preserved_in_output() {
        // source "a\n" -> "  a\n  " (split on \n yields ["a", ""]).
        // This pins the per-line indent contract for stream-formed input.
        let out = highlight_block(None, "a\n", caps_nocolor());
        assert_eq!(out, "  a\n  ");
    }
}
