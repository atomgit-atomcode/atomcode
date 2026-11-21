//! A change, rendered as coloured, line-numbered rows.
//!
//! Two producers reach the same rows. `edit_file` returns a unified diff in its
//! result (`Edited …` then `@@ -a,b +c,d @@` hunks with ` `/`+`/`-` lines); this
//! parses it, ignoring everything before the first hunk (the `Edited …`
//! preamble, the `---`/`+++` headers). `write_file` returns no diff, so its
//! freshly-written content is shown as all-additions — a new file is all green,
//! an overwrite shows what it now holds.
//!
//! The parser is deliberately the same shape the other front end's is, so a diff
//! reads the same across both screens. Rendering here is spans (green add, red
//! remove, dim context and line numbers) rather than pre-baked escapes, because
//! this tree measures what it draws.

use crate::frame::{Color, Line, Span, Style};
use crate::theme::Role;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Add,
    Del,
    Context,
    /// A gap of unchanged lines between two hunks, drawn as a `⋮`.
    Separator,
}

struct Entry {
    kind: Kind,
    old_lineno: Option<usize>,
    new_lineno: Option<usize>,
    text: String,
}

/// What a diff render produced: the rows, how many lines it added and removed,
/// and whether more was cut for length.
pub struct Rendered {
    pub lines: Vec<Line>,
    pub added: usize,
    pub removed: usize,
}

/// Render an `edit_file` result — the unified diff inside `output`, coloured and
/// line-numbered under `indent`. `None` when `output` carries no change to show
/// (no hunk, or context only), so a caller falls back to plain text.
pub fn render_edit(output: &str, w: u16, indent: &str, max: usize) -> Option<Rendered> {
    render_edit_with_wrap(output, w, indent, max, true)
}

pub fn render_edit_with_wrap(
    output: &str,
    w: u16,
    indent: &str,
    max: usize,
    wrap: bool,
) -> Option<Rendered> {
    let entries = parse_unified(output, max);
    if entries
        .iter()
        .all(|e| matches!(e.kind, Kind::Context | Kind::Separator))
    {
        return None;
    }
    Some(render(&entries, w, indent, wrap))
}

/// Render a `write_file` result — every line of `content` as an addition.
pub fn render_written(content: &str, w: u16, indent: &str, max: usize) -> Rendered {
    render_written_with_wrap(content, w, indent, max, true)
}

pub fn render_written_with_wrap(
    content: &str,
    w: u16,
    indent: &str,
    max: usize,
    wrap: bool,
) -> Rendered {
    let entries: Vec<Entry> = content
        .lines()
        .take(max)
        .enumerate()
        .map(|(i, text)| Entry {
            kind: Kind::Add,
            old_lineno: None,
            new_lineno: Some(i + 1),
            text: text.to_string(),
        })
        .collect();
    render(&entries, w, indent, wrap)
}

fn render(entries: &[Entry], w: u16, indent: &str, wrap: bool) -> Rendered {
    let gutter = gutter_width(entries);
    let added = entries.iter().filter(|e| e.kind == Kind::Add).count();
    let removed = entries.iter().filter(|e| e.kind == Kind::Del).count();
    let lines = entries
        .iter()
        .flat_map(|e| row(e, gutter, indent, w, wrap))
        .collect();
    Rendered {
        lines,
        added,
        removed,
    }
}

/// `{indent}{num:>gutter} {sign} {text}` — the number dim, the change coloured.
fn row(e: &Entry, gutter: usize, indent: &str, w: u16, wrap: bool) -> Vec<Line> {
    if e.kind == Kind::Separator {
        return vec![Line::from_spans(vec![Span::styled(
            format!("{indent}{:>gutter$} \u{22ee}", ""),
            dim(),
        )])
        .truncate(w as usize)];
    }
    let num = match e.kind {
        Kind::Del => e.old_lineno,
        _ => e.new_lineno,
    };
    let numstr = num.map(|n| n.to_string()).unwrap_or_default();
    let sign = match e.kind {
        Kind::Add => '+',
        Kind::Del => '-',
        _ => ' ',
    };
    let style = match e.kind {
        Kind::Add => Style::new().fg(Color::role(Role::DiffAdd)),
        Kind::Del => Style::new().fg(Color::role(Role::DiffRemove)),
        _ => dim(),
    };
    display_row(
        &e.text,
        &format!("{indent}{numstr:>gutter$} {sign} "),
        &format!("{indent}{:>gutter$} > ", ""),
        w,
        style,
        wrap,
    )
}

/// Shared by inline tool diffs and the /diff reader. Continuation prefixes
/// belong to the display, never to the source or clipboard.
pub(crate) fn display_row(
    text: &str,
    first: &str,
    next: &str,
    w: u16,
    style: Style,
    wrap: bool,
) -> Vec<Line> {
    if w == 0 {
        return Vec::new();
    }
    let text = crate::text::for_screen(text);
    if !wrap {
        let prefix = Line::styled(first.to_owned(), dim());
        let prefix = prefix.restyle(prefix.width().saturating_sub(2), prefix.width(), |_| style);
        let mut spans = prefix.spans;
        spans.push(Span::styled(text.into_owned(), style));
        return vec![Line::from_spans(spans).truncate(w as usize)];
    }
    // Reserve body space even when a line-number gutter exceeds the terminal.
    let prefix_room = (w as usize).saturating_sub(2);
    let first = Line::raw(first).truncate(prefix_room);
    let next = Line::raw(next).truncate(prefix_room);
    let body = w
        .saturating_sub(first.width().max(next.width()) as u16)
        .max(1);
    crate::markdown::wrap_spans(&[Span::styled(text.into_owned(), style)], body, "", style)
        .into_iter()
        .enumerate()
        .map(|(at, row)| {
            let mut line = if at == 0 { first.clone() } else { next.clone() };
            for span in &mut line.spans {
                span.style = dim();
            }
            if at == 0 {
                line = line.restyle(line.width().saturating_sub(2), line.width(), |_| style);
            }
            line.spans.extend(row.spans);
            line
        })
        .collect()
}

fn dim() -> Style {
    Style::new().fg(Color::role(Role::Muted))
}

/// The digit count of the largest line number shown (Del shows old, else new),
/// at least 1 — so every number right-aligns in one column.
fn gutter_width(entries: &[Entry]) -> usize {
    entries
        .iter()
        .filter_map(|e| match e.kind {
            Kind::Del => e.old_lineno,
            _ => e.new_lineno,
        })
        .max()
        .map(|n| n.to_string().len())
        .unwrap_or(1)
        .max(1)
}

/// Parse a unified diff into line-numbered entries, ignoring the preamble before
/// the first `@@` hunk. Stops after `max` entries.
fn parse_unified(diff: &str, max: usize) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut old_ln = 0usize;
    let mut new_ln = 0usize;
    // Whether the first `@@` hunk has been seen — the boundary between preamble
    // and body. A `bool`, NOT `old_ln == 0 && new_ln == 0`: a degenerate
    // `@@ -0,0 +0,0 @@` header sets both counters to 0, and overloading that as
    // "still in the preamble" would drop the hunk's own body.
    let mut seen_hunk = false;
    for line in diff.lines() {
        if out.len() >= max {
            break;
        }
        if let Some(rest) = line.strip_prefix("@@") {
            if let Some((o, n)) = parse_hunk_header(rest) {
                // A later hunk means a gap of unchanged lines was elided: mark it
                // so the two hunks read as one file block rather than run together.
                if !out.is_empty() {
                    out.push(Entry {
                        kind: Kind::Separator,
                        old_lineno: None,
                        new_lineno: None,
                        text: String::new(),
                    });
                }
                old_ln = o;
                new_ln = n;
                seen_hunk = true;
            }
            continue;
        }
        if !seen_hunk {
            // Preamble before the first hunk: the `Edited …` line and the
            // `---`/`+++` file headers. NOT skipped inside a hunk — a `-- ` line
            // that diffs to `--- ` is deleted content, and reaches the arms below.
            continue;
        }
        // The prefix byte (`+`/`-`/` `) is ASCII, so `line[1..]` is a boundary.
        #[allow(
            clippy::string_slice,
            reason = "the unified-diff prefix is one ASCII byte"
        )]
        match line.as_bytes().first() {
            Some(b'+') => {
                out.push(Entry {
                    kind: Kind::Add,
                    old_lineno: None,
                    new_lineno: Some(new_ln),
                    text: line[1..].to_string(),
                });
                new_ln += 1;
            }
            Some(b'-') => {
                out.push(Entry {
                    kind: Kind::Del,
                    old_lineno: Some(old_ln),
                    new_lineno: None,
                    text: line[1..].to_string(),
                });
                old_ln += 1;
            }
            Some(b' ') => {
                out.push(Entry {
                    kind: Kind::Context,
                    old_lineno: Some(old_ln),
                    new_lineno: Some(new_ln),
                    text: line[1..].to_string(),
                });
                old_ln += 1;
                new_ln += 1;
            }
            _ => {} // `\ No newline at end of file`, a blank line, the `… N more` tail.
        }
    }
    out
}

/// The two 1-based start line numbers from a hunk header body (`rest` = the text
/// after `@@`, e.g. ` -12,3 +14,4 @@ …`).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_wrap_without_losing_text_or_changing_counts() {
        let text = "https://example.com/a/very/long/path?query=abcdefghijk";
        let shown = render_written(text, 18, "", 10);
        assert_eq!(shown.added, 1);
        assert_eq!(shown.removed, 0);
        assert!(shown.lines.len() > 1);
        assert!(shown.lines.iter().all(|line| line.width() <= 18));
        assert!(shown
            .lines
            .iter()
            .skip(1)
            .all(|line| line.plain().starts_with("  > ")));
        let restored: String = shown
            .lines
            .iter()
            .map(|line| line.plain().chars().skip(4).collect::<String>())
            .collect();
        assert_eq!(restored, text);
        let clipped = render_written_with_wrap(text, 18, "", 10, false);
        assert_eq!(clipped.lines.len(), 1);
        assert!(!clipped.lines[0].plain().contains("abcdefghijk"));
    }

    #[test]
    fn narrow_unicode_diffs_keep_the_tail_and_their_colours() {
        let source = "中文宽字符和e\u{301}组合字符到末尾";
        for width in [2, 10, 24] {
            let shown = render_written(source, width, "", 10);
            assert!(shown
                .lines
                .iter()
                .all(|line| line.width() <= width as usize));
            let body: String = shown
                .lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .filter(|span| span.style.fg == Some(Color::role(Role::DiffAdd)))
                .map(|span| span.text.as_str())
                .collect();
            assert_eq!(
                body.strip_prefix("+ ").unwrap_or(&body),
                source,
                "width {width}"
            );
        }
        assert!(render_written(source, 0, "", 10).lines.is_empty());
    }

    /// The plain text of a rendered row (spans concatenated), trimmed.
    fn texts(r: &Rendered) -> Vec<String> {
        r.lines
            .iter()
            .map(|l| l.plain().trim_end().to_string())
            .collect()
    }

    #[test]
    fn an_edit_diff_is_parsed_coloured_and_line_numbered() {
        // The `Edited …` preamble is dropped; the hunk becomes numbered rows.
        let output = "Edited a.rs (1 replacement)\n\
                      @@ -1,3 +1,3 @@\n \
                      keep\n\
                      -old line\n\
                      +new line\n \
                      tail";
        let r = render_edit(output, 80, "  ", 200).expect("a diff");
        assert_eq!(r.added, 1);
        assert_eq!(r.removed, 1);
        let rows = texts(&r);
        // context(1) + del(1) + add(2) + context(3) — line numbers off the hunk.
        assert!(
            rows[0].contains("1") && rows[0].contains("keep"),
            "{rows:?}"
        );
        assert!(
            rows[1].contains("2") && rows[1].contains("- old line"),
            "{rows:?}"
        );
        assert!(
            rows[2].contains("2") && rows[2].contains("+ new line"),
            "{rows:?}"
        );
    }

    #[test]
    fn the_del_row_is_red_and_the_add_row_is_green() {
        let output = "@@ -1,1 +1,1 @@\n-a\n+b";
        let r = render_edit(output, 80, "", 200).unwrap();
        let del = &r.lines[0];
        let add = &r.lines[1];
        let red = Some(Color::role(Role::DiffRemove));
        let green = Some(Color::role(Role::DiffAdd));
        assert!(del.spans.iter().any(|s| s.style.fg == red), "del is red");
        assert!(
            add.spans.iter().any(|s| s.style.fg == green),
            "add is green"
        );
    }

    #[test]
    fn a_context_only_diff_is_not_worth_colour() {
        // Nothing changed to show → None, and the caller shows plain text.
        assert!(render_edit("@@ -1,1 +1,1 @@\n unchanged", 80, "", 200).is_none());
        assert!(render_edit("Edited a.rs (0 replacements)", 80, "", 200).is_none());
    }

    #[test]
    fn a_written_file_is_all_additions() {
        let r = render_written("line one\nline two\nline three", 80, "  ", 200);
        assert_eq!(r.added, 3);
        assert_eq!(r.removed, 0);
        let rows = texts(&r);
        assert!(
            rows[0].contains("1") && rows[0].contains("+ line one"),
            "{rows:?}"
        );
        assert!(
            rows[2].contains("3") && rows[2].contains("+ line three"),
            "{rows:?}"
        );
    }

    #[test]
    fn a_second_hunk_gets_a_separator() {
        let output = "@@ -1,1 +1,1 @@\n+a\n@@ -9,1 +9,1 @@\n+b";
        let r = render_edit(output, 80, "", 200).unwrap();
        let rows = texts(&r);
        assert!(
            rows.iter().any(|l| l.contains('\u{22ee}')),
            "a ⋮ gap: {rows:?}"
        );
        assert_eq!(r.added, 2);
    }

    #[test]
    fn a_long_change_stops_at_max() {
        let big: String = (0..500).map(|i| format!("l{i}\n")).collect();
        let r = render_written(&big, 80, "", 50);
        assert_eq!(r.lines.len(), 50, "capped at max");
    }
}
