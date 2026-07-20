// crates/atomcode-tuix/src/highlight/mod.rs
//
// Fenced-code-block formatter. Indents each source line with two spaces
// and emits no per-token colour.
//
// History: this module used to drive `syntect` for per-token syntax
// highlighting. The truecolor tints (purple keywords, blue function
// names, sand type names, etc.) composited against macOS Terminal.app's
// semi-transparent grey selection overlay to luminance values
// indistinguishable from the overlay itself — selecting a code block
// made most tokens invisible. Default fg survives the overlay because
// the terminal flips it to a high-contrast counterpart. The fix —
// drop per-token colour entirely — matches `opencode`'s TUI choice
// (`markdownCodeBlock: fg`) and is universal across emulators. See
// git history for the removed syntect path if it ever needs reviving.

pub mod theme;

/// Format a fenced code block: 2-space left indent, default fg, no ANSI.
/// Callers (`markdown.rs`) splice the returned string into the body stream.
pub fn highlight_block(source: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_gets_2_space_indent() {
        assert_eq!(highlight_block("let x = 1;"), "  let x = 1;");
    }

    #[test]
    fn multi_line_each_line_indented() {
        assert_eq!(
            highlight_block("let x = 1;\nlet y = 2;"),
            "  let x = 1;\n  let y = 2;"
        );
    }

    #[test]
    fn empty_source_returns_indent_only() {
        assert_eq!(highlight_block(""), "  ");
    }

    #[test]
    fn trailing_newline_preserved() {
        // "a\n".split('\n') == ["a", ""] → "  a\n  ". Pins the per-line
        // indent contract for stream-formed input where the close-fence
        // flush leaves a trailing newline.
        assert_eq!(highlight_block("a\n"), "  a\n  ");
    }

    #[test]
    fn zero_ansi_for_codeish_input() {
        // Plan-0: even input that historically would have been syntect-
        // highlighted (looks like rust / has keywords / has comments)
        // emits zero ANSI under the new contract.
        let src = "fn main() { let x = 1; }\n// a comment\nlet s = \"hi\";";
        let out = highlight_block(src);
        assert!(
            !out.contains('\x1b'),
            "expected zero ANSI bytes, got: {out:?}"
        );
        for (i, line) in out.split('\n').enumerate() {
            assert!(
                line.starts_with("  "),
                "line {i} missing 2-space indent: {line:?}"
            );
        }
    }
}
