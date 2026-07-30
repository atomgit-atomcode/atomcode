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

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

pub mod theme;

/// Format a fenced code block: 2-space left indent, default fg, no ANSI.
/// Callers (`markdown.rs`) splice the returned string into the body stream.
pub fn highlight_block(source: &str) -> String {
    let display = normalize_cjk_diagram_for_display(source);
    let mut out = String::with_capacity(source.len() + 32);
    let mut first = true;
    for line in display.split('\n') {
        if !first {
            out.push('\n');
        }
        out.push_str("  ");
        out.push_str(line);
        first = false;
    }
    out
}

/// Repair model-generated box diagrams that were laid out by character index
/// instead of terminal column width.
///
/// Models commonly count a CJK glyph as one character when padding an ASCII /
/// Unicode box, while terminals render it as two columns. The trailing border
/// then drifts right by one column for every CJK glyph. For diagram-like fenced
/// blocks only, remove enough padding immediately before a vertical boundary to
/// put that boundary back at its source character column.
///
/// This is display-only: markdown keeps the original `code_buf` separately for
/// `/copy` and session persistence. Ordinary source-code blocks are left byte
/// for byte unchanged by the conservative diagram detector.
fn normalize_cjk_diagram_for_display(source: &str) -> Cow<'_, str> {
    let correction_columns = cjk_diagram_correction_columns(source);
    if correction_columns.is_empty() {
        return Cow::Borrowed(source);
    }

    Cow::Owned(
        source
            .split('\n')
            .map(|line| normalize_cjk_diagram_line(line, &correction_columns))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn cjk_diagram_correction_columns(source: &str) -> HashSet<usize> {
    let has_wide_text = source
        .chars()
        .any(|ch| !ch.is_ascii() && crate::width::display_width(ch.encode_utf8(&mut [0; 4])) > 1);
    if !has_wide_text {
        return HashSet::new();
    }

    let mut structural_lines = 0usize;
    let mut pipe_rows = 0usize;
    let mut has_box_corner = false;
    let mut has_arrow = false;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.chars().any(is_diagram_structure) {
            structural_lines += 1;
        }
        if (trimmed.starts_with('|') && trimmed.ends_with('|'))
            || (trimmed.starts_with('│') && trimmed.ends_with('│'))
        {
            pipe_rows += 1;
        }
        has_box_corner |= trimmed
            .chars()
            .any(|ch| matches!(ch, '┌' | '┐' | '└' | '┘' | '╭' | '╮' | '╰' | '╯'));
        has_arrow |= trimmed
            .chars()
            .any(|ch| matches!(ch, '↑' | '↓' | '←' | '→' | '▲' | '▼' | '◀' | '▶'));
    }

    if structural_lines < 3 || !(has_box_corner || pipe_rows >= 2 || has_arrow) {
        return HashSet::new();
    }

    // Evidence, not symbol presence, decides whether display repair is needed.
    // A model-laid-out diagram puts matching borders at the same *character*
    // column. CJK text makes those borders land at different terminal columns.
    // Conversely, a diagram already padded by display width has matching
    // terminal columns but different character columns and must be untouched.
    #[derive(Default)]
    struct BoundaryEvidence {
        count: usize,
        min_display_col: usize,
        max_display_col: usize,
    }

    let mut by_source_col: HashMap<usize, BoundaryEvidence> = HashMap::new();
    for line in source.lines() {
        let mut source_col = 0usize;
        let mut display_col = 0usize;
        for ch in line.chars() {
            if is_vertical_boundary(ch) {
                let evidence =
                    by_source_col
                        .entry(source_col)
                        .or_insert_with(|| BoundaryEvidence {
                            min_display_col: display_col,
                            max_display_col: display_col,
                            ..BoundaryEvidence::default()
                        });
                evidence.count += 1;
                evidence.min_display_col = evidence.min_display_col.min(display_col);
                evidence.max_display_col = evidence.max_display_col.max(display_col);
            }
            source_col += 1;
            display_col += crate::width::display_width(ch.encode_utf8(&mut [0; 4]));
        }
    }

    by_source_col
        .into_iter()
        .filter_map(|(source_col, evidence)| {
            (evidence.count >= 3 && evidence.min_display_col < evidence.max_display_col)
                .then_some(source_col)
        })
        .collect()
}

fn normalize_cjk_diagram_line(line: &str, correction_columns: &HashSet<usize>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut source_col = 0usize;
    let mut display_col = 0usize;

    for ch in line.chars() {
        if is_vertical_boundary(ch) && correction_columns.contains(&source_col) {
            let mut excess = display_col.saturating_sub(source_col);
            while excess > 0 && out.ends_with(' ') {
                out.pop();
                display_col = display_col.saturating_sub(1);
                excess -= 1;
            }
        }

        out.push(ch);
        source_col += 1;
        display_col += crate::width::display_width(ch.encode_utf8(&mut [0; 4]));
    }

    out
}

fn is_diagram_structure(ch: char) -> bool {
    is_vertical_boundary(ch)
        || matches!(
            ch,
            '─' | '━'
                | '┬'
                | '┴'
                | '┼'
                | '╦'
                | '╩'
                | '╬'
                | '↑'
                | '↓'
                | '←'
                | '→'
                | '▲'
                | '▼'
                | '◀'
                | '▶'
        )
}

fn is_vertical_boundary(ch: char) -> bool {
    matches!(
        ch,
        '|' | '│'
            | '┃'
            | '┌'
            | '┐'
            | '└'
            | '┘'
            | '├'
            | '┤'
            | '╭'
            | '╮'
            | '╰'
            | '╯'
            | '╞'
            | '╡'
    )
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

    #[test]
    fn cjk_box_diagram_aligns_vertical_boundaries_by_display_width() {
        // Every source row has the same *character count*: this is the shape
        // models emit when they mistakenly count each CJK glyph as one column.
        let source =
            "┌────────────┐\n│ config     │\n│ 配置         │\n│ auth       │\n└────────────┘";
        let out = highlight_block(source);
        let widths = out
            .lines()
            .map(crate::width::display_width)
            .collect::<Vec<_>>();

        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "all box rows must occupy the same terminal width: {widths:?}\n{out}"
        );
        assert!(out.contains("  │ 配置       │"), "{out}");
    }

    #[test]
    fn cjk_source_code_is_not_rewritten_without_diagram_structure() {
        let source = "fn main() {\n    // 中文注释    保留空格\n    println!(\"│\");\n}";
        assert_eq!(
            highlight_block(source),
            "  fn main() {\n      // 中文注释    保留空格\n      println!(\"│\");\n  }"
        );
    }

    #[test]
    fn already_display_aligned_cjk_diagram_is_not_rewritten() {
        let source =
            "┌────────────┐\n│ config     │\n│ 配置       │\n│ auth       │\n└────────────┘";
        assert_eq!(
            highlight_block(source),
            "  ┌────────────┐\n  │ config     │\n  │ 配置       │\n  │ auth       │\n  └────────────┘"
        );
    }

    #[test]
    fn code_with_cjk_pipes_and_arrow_is_not_rewritten_without_drift_evidence() {
        let source =
            "let rows = [\n    \"| 中文 → |\",\n    \"| English  |\",\n    \"| another  |\",\n];";
        assert_eq!(
            highlight_block(source),
            "  let rows = [\n      \"| 中文 → |\",\n      \"| English  |\",\n      \"| another  |\",\n  ];"
        );
    }

    #[test]
    fn ordinary_code_block_uses_borrowed_display_source() {
        assert!(matches!(
            normalize_cjk_diagram_for_display("fn main() {}"),
            Cow::Borrowed(_)
        ));
    }
}
