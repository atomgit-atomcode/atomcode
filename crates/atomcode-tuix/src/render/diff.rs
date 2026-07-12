//! Pure diff-parsing and row-formatting logic for `UiLine::DiffBlock`.
//! Rendering (cells/SGR) lives in retained.rs / plain.rs; this module only
//! turns a unified-diff string into line-numbered entries and formats a row.

use crate::render::{DiffEntry, DiffKind};

/// Parse a git unified diff (`@@ -a,b +c,d @@` hunks + ` `/`+`/`-` lines) into
/// line-numbered entries. Lines before the first `@@`, and `---`/`+++` file
/// headers, are ignored. Stops after `max_lines` entries.
pub(crate) fn parse_unified_diff(diff: &str, max_lines: usize) -> Vec<DiffEntry> {
    let mut out: Vec<DiffEntry> = Vec::new();
    let mut old_ln = 0usize;
    let mut new_ln = 0usize;
    for line in diff.lines() {
        if out.len() >= max_lines {
            break;
        }
        if let Some(rest) = line.strip_prefix("@@") {
            if let Some((o, n)) = parse_hunk_header(rest) {
                old_ln = o;
                new_ln = n;
            }
            continue;
        }
        if line.starts_with("---") || line.starts_with("+++") {
            continue; // unified-diff file headers
        }
        if old_ln == 0 && new_ln == 0 {
            continue; // preamble before the first hunk
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                out.push(DiffEntry {
                    kind: DiffKind::Add,
                    old_lineno: None,
                    new_lineno: Some(new_ln),
                    text: line[1..].to_string(),
                });
                new_ln += 1;
            }
            Some(b'-') => {
                out.push(DiffEntry {
                    kind: DiffKind::Del,
                    old_lineno: Some(old_ln),
                    new_lineno: None,
                    text: line[1..].to_string(),
                });
                old_ln += 1;
            }
            Some(b' ') => {
                out.push(DiffEntry {
                    kind: DiffKind::Context,
                    old_lineno: Some(old_ln),
                    new_lineno: Some(new_ln),
                    text: line[1..].to_string(),
                });
                old_ln += 1;
                new_ln += 1;
            }
            _ => {} // `\ No newline at end of file`, blank lines, etc.
        }
    }
    out
}

/// Parse the two 1-based start line numbers from a hunk header body
/// (`rest` = the text after `@@`, e.g. ` -12,3 +14,4 @@ …`).
fn parse_hunk_header(rest: &str) -> Option<(usize, usize)> {
    let mut old_start = None;
    let mut new_start = None;
    for tok in rest.split_whitespace() {
        if let Some(o) = tok.strip_prefix('-') {
            old_start = o.split(',').next().and_then(|s| s.parse::<usize>().ok());
        } else if let Some(n) = tok.strip_prefix('+') {
            new_start = n.split(',').next().and_then(|s| s.parse::<usize>().ok());
        }
    }
    Some((old_start?, new_start?))
}

/// Width of the line-number gutter: the digit count of the largest line number
/// shown across `entries` (Del shows old, others show new), minimum 1.
pub(crate) fn diff_gutter_width(entries: &[DiffEntry]) -> usize {
    entries
        .iter()
        .filter_map(|e| match e.kind {
            DiffKind::Del => e.old_lineno,
            _ => e.new_lineno,
        })
        .max()
        .unwrap_or(1)
        .to_string()
        .len()
        .max(1)
}

/// Format one diff row as `"  {num:>gutter} {sign} {text}"` — the display line
/// (WITHOUT color and WITHOUT control-scrubbing; the caller applies the theme
/// role and scrubs controls). `text` is the raw entry text.
pub(crate) fn diff_row_text(entry: &DiffEntry, gutter: usize) -> String {
    let num = match entry.kind {
        DiffKind::Del => entry.old_lineno,
        _ => entry.new_lineno,
    };
    let numstr = num.map(|n| n.to_string()).unwrap_or_default();
    let sign = match entry.kind {
        DiffKind::Add => '+',
        DiffKind::Del => '-',
        DiffKind::Context => ' ',
    };
    format!("  {numstr:>gutter$} {sign} {}", entry.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunk_line_numbers_and_kinds() {
        let diff = "\
@@ -1,3 +1,3 @@
 fn main() {
-    let x = 1;
+    let x = 2;
 }";
        let e = parse_unified_diff(diff, 100);
        assert_eq!(e.len(), 4);
        assert_eq!(e[0].kind, DiffKind::Context);
        assert_eq!((e[0].old_lineno, e[0].new_lineno), (Some(1), Some(1)));
        assert_eq!(e[1].kind, DiffKind::Del);
        assert_eq!((e[1].old_lineno, e[1].new_lineno), (Some(2), None));
        assert_eq!(e[1].text, "    let x = 1;");
        assert_eq!(e[2].kind, DiffKind::Add);
        assert_eq!((e[2].old_lineno, e[2].new_lineno), (None, Some(2)));
        assert_eq!(e[3].kind, DiffKind::Context);
        assert_eq!((e[3].old_lineno, e[3].new_lineno), (Some(3), Some(3)));
    }

    #[test]
    fn ignores_preamble_and_file_headers() {
        let diff = "\
Edited a.rs (1 replacement)
--- a/a.rs
+++ b/a.rs
@@ -2,1 +2,1 @@
-old
+new";
        let e = parse_unified_diff(diff, 100);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].kind, DiffKind::Del);
        assert_eq!(e[0].text, "old");
        assert_eq!(e[1].kind, DiffKind::Add);
        assert_eq!(e[1].text, "new");
    }

    #[test]
    fn respects_max_lines() {
        let mut diff = String::from("@@ -1,0 +1,50 @@\n");
        for i in 0..50 {
            diff.push_str(&format!("+line {i}\n"));
        }
        let e = parse_unified_diff(&diff, 10);
        assert_eq!(e.len(), 10);
    }

    #[test]
    fn gutter_width_and_row_format() {
        let entries = vec![
            DiffEntry { kind: DiffKind::Context, old_lineno: Some(9), new_lineno: Some(9), text: "ctx".into() },
            DiffEntry { kind: DiffKind::Add, old_lineno: None, new_lineno: Some(10), text: "added".into() },
            DiffEntry { kind: DiffKind::Del, old_lineno: Some(10), new_lineno: None, text: "removed".into() },
        ];
        let w = diff_gutter_width(&entries);
        assert_eq!(w, 2); // largest line number is 10 → width 2
        assert_eq!(diff_row_text(&entries[0], w), "   9   ctx");
        assert_eq!(diff_row_text(&entries[1], w), "  10 + added");
        assert_eq!(diff_row_text(&entries[2], w), "  10 - removed");
    }
}
