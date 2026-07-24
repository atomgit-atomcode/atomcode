//! Pure diff-parsing and row-formatting logic for `UiLine::DiffBlock`.
//! Rendering (cells/SGR) lives in retained.rs / plain.rs; this module only
//! turns a unified-diff string into line-numbered entries and formats a row.

use crate::render::{DiffEntry, DiffKind};

#[derive(Debug, Clone)]
pub(crate) struct ParsedDiffFile {
    pub entries: Vec<DiffEntry>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedDiff {
    pub files: Vec<ParsedDiffFile>,
    pub truncated: bool,
}

/// Split a regular `git diff` stream into file-scoped blocks. The existing
/// hunk parser remains the single source of line-number and separator semantics.
pub(crate) fn parse_unified_diff_files(diff: &str, max_entries: usize) -> ParsedDiff {
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Option<String> = None;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            if let Some(chunk) = current.take() {
                chunks.push(chunk);
            }
            current = Some(String::new());
        } else if let Some(body) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(chunk) = current {
        chunks.push(chunk);
    }
    if chunks.is_empty() && !diff.trim().is_empty() {
        chunks.push(diff.to_string());
    }

    let mut files = Vec::with_capacity(chunks.len());
    let mut total_entries = 0usize;
    let mut truncated = false;
    for body in chunks {
        let remaining = max_entries.saturating_sub(total_entries);
        let has_hunk = body.lines().any(|line| line.starts_with("@@"));
        let mut entries = if remaining == 0 {
            Vec::new()
        } else {
            parse_unified_diff(&body, remaining.saturating_add(2))
        };
        if entries.len() > remaining {
            entries.truncate(remaining);
            truncated = true;
        } else if remaining == 0 && has_hunk {
            truncated = true;
        }
        total_entries = total_entries.saturating_add(entries.len());

        let summary_lines: Vec<&str> = body
            .lines()
            .filter(|line| {
                line.starts_with("Binary files ")
                    || *line == "GIT binary patch"
                    || line.starts_with("old mode ")
                    || line.starts_with("new mode ")
                    || line.starts_with("deleted file mode ")
                    || line.starts_with("new file mode ")
                    || line.starts_with("similarity index ")
                    || line.starts_with("rename from ")
                    || line.starts_with("rename to ")
            })
            .take(8)
            .collect();
        let summary = (!summary_lines.is_empty()).then(|| summary_lines.join("\n"));

        files.push(ParsedDiffFile { entries, summary });
    }

    ParsedDiff { files, truncated }
}

/// Parse a git unified diff (`@@ -a,b +c,d @@` hunks + ` `/`+`/`-` lines) into
/// line-numbered entries. Everything before the first `@@` hunk — the `Edited …`
/// preamble and any `--- `/`+++ ` file headers — is ignored. Stops after
/// `max_lines` entries.
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
                // A second/later hunk = a gap of unchanged lines was elided.
                // Mark it with a `⋮` separator so the two hunks read as one file
                // block (codex style) instead of running together.
                if !out.is_empty() {
                    out.push(DiffEntry {
                        kind: DiffKind::Separator,
                        old_lineno: None,
                        new_lineno: None,
                        text: String::new(),
                    });
                }
                old_ln = o;
                new_ln = n;
            }
            continue;
        }
        if old_ln == 0 && new_ln == 0 {
            // Preamble before the first hunk: the `Edited …` line and the
            // `--- `/`+++ ` file headers (which always precede the first `@@`).
            // Do NOT skip `---`/`+++`-shaped lines INSIDE a hunk — those are
            // deleted/added content (e.g. a `-- ` line diffs to `--- `).
            continue;
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
    // Hunk-gap marker: a dim `⋮` in the sign column, blank gutter. (The retained
    // renderer downgrades `⋮` to `...` on non-unicode terminals.)
    if entry.kind == DiffKind::Separator {
        return format!("  {:>gutter$} \u{22ee}", "");
    }
    let num = match entry.kind {
        DiffKind::Del => entry.old_lineno,
        _ => entry.new_lineno,
    };
    let numstr = num.map(|n| n.to_string()).unwrap_or_default();
    let sign = match entry.kind {
        DiffKind::Add => '+',
        DiffKind::Del => '-',
        _ => ' ',
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
    fn del_line_starting_with_dashes_is_not_dropped_as_a_header() {
        // A deleted line whose content starts with `--` becomes `---…` in the
        // diff (sign `-` + content `--…`). It must render as a DELETION, not be
        // mistaken for a `---` file header and silently dropped (which would also
        // throw off every following line number). Same idea for added `++` lines.
        let diff = "\
@@ -1,3 +1,2 @@
 keep
---
+++new";
        let e = parse_unified_diff(diff, 100);
        assert_eq!(e.len(), 3, "context + del + add, none dropped: {e:?}");
        assert_eq!(e[0].kind, DiffKind::Context);
        assert_eq!(
            (e[1].kind, e[1].text.as_str(), e[1].old_lineno),
            (DiffKind::Del, "--", Some(2))
        );
        assert_eq!(
            (e[2].kind, e[2].text.as_str(), e[2].new_lineno),
            (DiffKind::Add, "++new", Some(2))
        );
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
    fn second_hunk_gets_a_separator() {
        let diff = "\
@@ -1,1 +1,1 @@
-a
+b
@@ -50,1 +50,1 @@
-c
+d";
        let e = parse_unified_diff(diff, 100);
        // del a / add b / SEPARATOR / del c / add d
        assert_eq!(e.len(), 5);
        assert_eq!(e[2].kind, DiffKind::Separator);
        assert_eq!((e[2].old_lineno, e[2].new_lineno), (None, None));
        // no leading separator before the first hunk
        assert_ne!(e[0].kind, DiffKind::Separator);
        // separator renders as a dim ⋮ with a blank gutter
        assert!(diff_row_text(&e[2], 2).trim_end().ends_with('\u{22ee}'));
    }

    #[test]
    fn splits_git_diff_by_file() {
        let diff = "\
diff --git a/a.txt b/a.txt
index 7898192..6178079 100644
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old-a
+new-a
diff --git a/b.txt b/b.txt
index 6178079..f2ad6c7 100644
--- a/b.txt
+++ b/b.txt
@@ -3 +3 @@
-old-b
+new-b";

        let parsed = parse_unified_diff_files(diff, 100);

        assert!(!parsed.truncated);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].entries.len(), 2);
        assert_eq!(parsed.files[0].entries[0].text, "old-a");
        assert_eq!(parsed.files[0].entries[1].text, "new-a");
        assert_eq!(parsed.files[1].entries[0].text, "old-b");
        assert_eq!(parsed.files[1].entries[1].text, "new-b");
    }

    #[test]
    fn file_parser_marks_entry_limit_and_keeps_binary_fallback() {
        let diff = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
-old-a
+new-a
-old-b
+new-b
diff --git a/image.png b/image.png
Binary files a/image.png and b/image.png differ";

        let parsed = parse_unified_diff_files(diff, 3);

        assert!(parsed.truncated);
        assert_eq!(parsed.files[0].entries.len(), 3);
        assert_eq!(
            parsed.files[1].summary.as_deref(),
            Some("Binary files a/image.png and b/image.png differ")
        );
    }

    #[test]
    fn gutter_width_and_row_format() {
        let entries = vec![
            DiffEntry {
                kind: DiffKind::Context,
                old_lineno: Some(9),
                new_lineno: Some(9),
                text: "ctx".into(),
            },
            DiffEntry {
                kind: DiffKind::Add,
                old_lineno: None,
                new_lineno: Some(10),
                text: "added".into(),
            },
            DiffEntry {
                kind: DiffKind::Del,
                old_lineno: Some(10),
                new_lineno: None,
                text: "removed".into(),
            },
        ];
        let w = diff_gutter_width(&entries);
        assert_eq!(w, 2); // largest line number is 10 → width 2
        assert_eq!(diff_row_text(&entries[0], w), "   9   ctx");
        assert_eq!(diff_row_text(&entries[1], w), "  10 + added");
        assert_eq!(diff_row_text(&entries[2], w), "  10 - removed");
    }
}
