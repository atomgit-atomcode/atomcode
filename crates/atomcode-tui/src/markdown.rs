//! Markdown, rendered into styled lines.
//!
//! Not a port of the existing renderer: that one produces ANSI strings a line
//! at a time, and this architecture needs structured spans. The difference is
//! not cosmetic — spans are what let the host check that nothing drew outside
//! its rect, and a pre-baked escape sequence cannot be measured or re-wrapped.
//!
//! Deliberately partial. It covers what a coding conversation actually
//! contains — fenced code, inline code, emphasis, headings, lists, quotes,
//! links — and stops there. Tables, footnotes and HTML are not rendered
//! specially; they come out as text, which is honest and readable, rather than
//! half-supported.

use crate::frame::{Color, Line, Span, Style};
use crate::theme::Role;
use crate::width;

fn code() -> Style {
    Style::new().fg(Color::role(Role::Warning))
}
fn heading() -> Style {
    Style::new().fg(Color::role(Role::Accent)).bold()
}
fn quote() -> Style {
    Style::new().fg(Color::role(Role::Accent))
}
fn bullet() -> Style {
    Style::new().fg(Color::role(Role::Border))
}
fn link() -> Style {
    Style::new().fg(Color::role(Role::Accent)).underline()
}
fn fence() -> Style {
    // A role, not SGR 2: see `content::muted`. The terminal's own idea of
    // "darker" is not a contrast ratio anybody in this tree can check.
    Style::new().fg(Color::role(Role::Muted))
}

/// Render a markdown document at `w` cells.
pub fn render(text: &str, w: u16, base: Style) -> Vec<Line> {
    render_settled(text, w, base).lines
}

/// What a render produced, and how much of it can never change again.
pub struct Rendered {
    pub lines: Vec<Line>,
    /// Byte offset in the source past which appending more text cannot change
    /// `lines[..settled_lines]`.
    pub settled: usize,
    /// How many of `lines` that prefix covers.
    pub settled_lines: usize,
}

/// Render `text`, and report the point past which it is settled.
///
/// The streaming answer is one block whose text only grows, and re-rendering
/// all of it every frame is what makes a frame cost the length of the answer.
/// Everything up to `settled` is fixed: it ends on a line boundary, outside any
/// fence. A caller may keep `lines[..settled_lines]` and render only the rest
/// next time, because appending source lines can change nothing before the last
/// boundary that was outside a fence.
///
/// An open fence is the one thing that is *not* settled, wherever it starts:
/// its body is emitted only when the fence closes, and the rule above it then
/// gains the language label — both retroactive. So `settled` stays before the
/// opener until the closing fence is seen.
pub fn render_settled(text: &str, w: u16, base: Style) -> Rendered {
    if w == 0 {
        return Rendered {
            lines: Vec::new(),
            settled: 0,
            settled_lines: 0,
        };
    }
    let mut out = Vec::new();
    let mut in_code: Option<String> = None;
    let mut code_lines: Vec<String> = Vec::new();
    // The offset of the last line boundary seen outside a fence, and how many
    // lines had been produced by then.
    let mut settled = 0usize;
    let mut settled_lines = 0usize;
    let mut off = 0usize;

    for raw in text.split('\n') {
        let line_end = off + raw.len();
        // A segment not followed by a newline is the still-arriving last line,
        // which more text can extend — so it is never a settled boundary.
        let complete = line_end < text.len();
        let trimmed = raw.trim_end();
        // Fences first: inside a block, nothing else is markdown.
        if let Some(rest) = trimmed.trim_start().strip_prefix("```") {
            match in_code.take() {
                Some(lang) => {
                    out.extend(code_block(&code_lines, &lang, w));
                    code_lines.clear();
                }
                None => in_code = Some(rest.trim().to_string()),
            }
        } else if in_code.is_some() {
            code_lines.push(trimmed.to_string());
        } else {
            let t = trimmed.trim_start();
            let indent = trimmed.len() - t.len();

            if t.is_empty() {
                out.push(Line::empty());
            } else if let Some((level, title)) = heading_of(t) {
                let hashes = "#".repeat(level as usize);
                out.extend(wrap_spans(
                    &inline(title, heading()),
                    w,
                    &format!("{hashes} "),
                    heading(),
                ));
            } else if is_rule(t) {
                out.push(Line::styled("─".repeat(w as usize), fence()));
            } else if let Some(body) = t.strip_prefix("> ").or_else(|| t.strip_prefix(">")) {
                out.extend(wrap_spans(&inline(body, quote()), w, "▏ ", quote()));
            } else if let Some((marker, body)) = list_item(t) {
                let lead = format!("{}{marker} ", " ".repeat(indent));
                out.extend(wrap_spans(&inline(body, base), w, &lead, bullet()));
            } else {
                out.extend(wrap_spans(
                    &inline(t, base),
                    w,
                    " ".repeat(indent).as_str(),
                    base,
                ));
            }
        }
        off = if complete { line_end + 1 } else { line_end };
        if complete && in_code.is_none() {
            settled = off;
            settled_lines = out.len();
        }
    }
    // An unterminated fence is common in a stream that is still arriving; show
    // what there is rather than swallowing it.
    if in_code.is_some() && !code_lines.is_empty() {
        out.extend(code_block(&code_lines, "", w));
    }
    Rendered {
        lines: out,
        settled,
        settled_lines,
    }
}

fn heading_of(t: &str) -> Option<(u8, &str)> {
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        #[allow(
            clippy::string_slice,
            reason = "`#` is one byte, so the count of them is a byte boundary"
        )]
        let rest = &t[hashes..];
        rest.strip_prefix(' ').map(|body| (hashes as u8, body))
    } else {
        None
    }
}

fn is_rule(t: &str) -> bool {
    let c = t.chars().next().unwrap_or(' ');
    matches!(c, '-' | '*' | '_') && t.len() >= 3 && t.chars().all(|x| x == c)
}

fn list_item(t: &str) -> Option<(String, &str)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(body) = t.strip_prefix(m) {
            return Some(("•".to_string(), body));
        }
    }
    // `1. ` and friends.
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        #[allow(
            clippy::string_slice,
            reason = "ASCII digits taken from the front: their byte length is a boundary"
        )]
        let after_digits = &t[digits.len()..];
        if let Some(body) = after_digits.strip_prefix(". ") {
            return Some((format!("{digits}."), body));
        }
    }
    None
}

/// Split one line into styled runs: `code`, **bold**, *italic*, [text](url).
fn inline(text: &str, base: Style) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;

    let flush = |buf: &mut String, out: &mut Vec<Span>| {
        if !buf.is_empty() {
            out.push(Span::styled(std::mem::take(buf), base));
        }
    };

    while i < chars.len() {
        match chars[i] {
            '`' => {
                if let Some(end) = find(&chars, i + 1, '`') {
                    flush(&mut buf, &mut out);
                    out.push(Span::styled(
                        chars[i + 1..end].iter().collect::<String>(),
                        code(),
                    ));
                    i = end + 1;
                    continue;
                }
            }
            '*' | '_' if i + 1 < chars.len() && chars[i + 1] == chars[i] => {
                let marker = chars[i];
                if let Some(end) = find_pair(&chars, i + 2, marker) {
                    flush(&mut buf, &mut out);
                    let inner: String = chars[i + 2..end].iter().collect();
                    out.push(Span::styled(inner, style_with_bold(base)));
                    i = end + 2;
                    continue;
                }
            }
            '*' | '_' => {
                let marker = chars[i];
                if let Some(end) = find(&chars, i + 1, marker) {
                    if end > i + 1 {
                        flush(&mut buf, &mut out);
                        let inner: String = chars[i + 1..end].iter().collect();
                        out.push(Span::styled(inner, style_with_italic(base)));
                        i = end + 1;
                        continue;
                    }
                }
            }
            '[' => {
                if let Some(close) = find(&chars, i + 1, ']') {
                    if chars.get(close + 1) == Some(&'(') {
                        if let Some(paren) = find(&chars, close + 2, ')') {
                            flush(&mut buf, &mut out);
                            let label: String = chars[i + 1..close].iter().collect();
                            out.push(Span::styled(label, link()));
                            i = paren + 1;
                            continue;
                        }
                    }
                }
            }
            _ => {}
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut out);
    if out.is_empty() {
        out.push(Span::styled(String::new(), base));
    }
    out
}

fn find(chars: &[char], from: usize, what: char) -> Option<usize> {
    chars
        .iter()
        .skip(from)
        .position(|c| *c == what)
        .map(|p| p + from)
}

fn find_pair(chars: &[char], from: usize, marker: char) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i] == marker && chars[i + 1] == marker {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn style_with_bold(mut s: Style) -> Style {
    s.bold = true;
    s
}
fn style_with_italic(mut s: Style) -> Style {
    s.italic = true;
    s
}

/// Wrap styled runs to `w`, keeping a prefix on the first line and an
/// equivalent indent on the rest.
fn wrap_spans(spans: &[Span], w: u16, prefix: &str, prefix_style: Style) -> Vec<Line> {
    let indent = width::str_width(prefix);
    let body = (w as usize).saturating_sub(indent).max(1);
    let mut out: Vec<Line> = Vec::new();
    let mut current = Line::from_spans(vec![Span::styled(prefix.to_string(), prefix_style)]);
    let mut used = 0usize;

    for span in spans {
        for word in span.text.split_inclusive(' ') {
            let ww = width::str_width(word);
            if used > 0 && used + ww > body {
                out.push(std::mem::replace(
                    &mut current,
                    Line::from_spans(vec![Span::styled(" ".repeat(indent), prefix_style)]),
                ));
                used = 0;
            }
            if ww > body {
                // A token longer than the line: hard-break it, never loop.
                let mut rest = word;
                while !rest.is_empty() {
                    let room = body - used;
                    let piece = width::take_width(rest, room);
                    if piece.is_empty() {
                        out.push(std::mem::replace(
                            &mut current,
                            Line::from_spans(vec![Span::styled(" ".repeat(indent), prefix_style)]),
                        ));
                        used = 0;
                        if room == body {
                            break; // cannot fit even on a fresh line
                        }
                        continue;
                    }
                    #[allow(
                        clippy::string_slice,
                        reason = "`piece` is a take_width prefix of `rest`, so its length is a boundary"
                    )]
                    {
                        rest = &rest[piece.len()..];
                    }
                    used += width::str_width(&piece);
                    current.push(Span::styled(piece, span.style));
                }
            } else {
                current.push(Span::styled(word.to_string(), span.style));
                used += ww;
            }
        }
    }
    out.push(current);
    out.into_iter().map(|l| l.truncate(w as usize)).collect()
}

/// A fenced block: dimmed rule, the source with keywords lit, another rule.
fn code_block(lines: &[String], lang: &str, w: u16) -> Vec<Line> {
    let mut out = Vec::new();
    let label = if lang.is_empty() {
        "─".repeat(w as usize)
    } else {
        let head = format!("─ {lang} ");
        format!(
            "{head}{}",
            "─".repeat((w as usize).saturating_sub(width::str_width(&head)))
        )
    };
    out.push(Line::styled(width::take_width(&label, w as usize), fence()));
    for line in lines {
        out.push(Line::from_spans(highlight(line, lang)).truncate(w as usize));
    }
    out.push(Line::styled("─".repeat(w as usize), fence()));
    out
}

const KEYWORDS: &[&str] = &[
    "fn",
    "let",
    "mut",
    "pub",
    "use",
    "impl",
    "struct",
    "enum",
    "trait",
    "match",
    "if",
    "else",
    "for",
    "while",
    "loop",
    "return",
    "async",
    "await",
    "const",
    "static",
    "type",
    "where",
    "self",
    "Self",
    "mod",
    "crate",
    "super",
    "as",
    "in",
    "ref",
    "move",
    "dyn",
    "unsafe",
    "def",
    "class",
    "import",
    "from",
    "lambda",
    "None",
    "True",
    "False",
    "elif",
    "try",
    "except",
    "with",
    "yield",
    "pass",
    "raise",
    "function",
    "var",
    "new",
    "this",
    "null",
    "undefined",
    "export",
    "default",
    "interface",
    "extends",
    "package",
    "func",
    "go",
    "defer",
    "nil",
    "range",
    "select",
    "case",
    "switch",
    "break",
    "continue",
    "do",
    "then",
    "fi",
    "esac",
];

/// Keyword-and-literal highlighting. Deliberately not a parser: a wrong colour
/// is a cosmetic problem, a wrong parse is a hang or a panic, and this runs on
/// every frame.
fn highlight(line: &str, _lang: &str) -> Vec<Span> {
    let kw = Style::new().fg(Color::role(Role::Brand));
    let string = Style::new().fg(Color::role(Role::Success));
    let comment = Style::new().fg(Color::role(Role::Border)).italic();
    let number = Style::new().fg(Color::role(Role::Warning));
    let plain = code();

    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with('#') || t.starts_with("--") {
        return vec![Span::styled(line.to_string(), comment)];
    }

    let mut out = Vec::new();
    let mut buf = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            if !buf.is_empty() {
                out.extend(word_spans(&std::mem::take(&mut buf), kw, number, plain));
            }
            let mut lit = String::from(c);
            for n in chars.by_ref() {
                lit.push(n);
                if n == c {
                    break;
                }
            }
            out.push(Span::styled(lit, string));
            continue;
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        out.extend(word_spans(&buf, kw, number, plain));
    }
    if out.is_empty() {
        out.push(Span::styled(line.to_string(), plain));
    }
    out
}

fn word_spans(text: &str, kw: Style, number: Style, plain: Style) -> Vec<Span> {
    let mut out = Vec::new();
    for token in text.split_inclusive(|c: char| !c.is_alphanumeric() && c != '_') {
        let word: String = token
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        #[allow(
            clippy::string_slice,
            reason = "`word` is taken from the front of `token`, so its byte length is a boundary"
        )]
        let tail = &token[word.len()..];
        if !word.is_empty() {
            let style = if KEYWORDS.contains(&word.as_str()) {
                kw
            } else if word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                number
            } else {
                plain
            };
            out.push(Span::styled(word, style));
        }
        if !tail.is_empty() {
            out.push(Span::styled(tail.to_string(), plain));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str, w: u16) -> Vec<String> {
        render(text, w, Style::new())
            .iter()
            .map(|l| l.plain())
            .collect()
    }

    /// Documents a streaming answer passes through, including every fence state
    /// and every partial-character boundary: the point of the settled render is
    /// that it can be resumed, and a corpus without a fence would not test that.
    const CORPUS: &[&str] = &[
        "one line, no newline",
        "one line\n",
        "first\nsecond\nthird\n",
        "para one\n\npara two\n\npara three\n",
        "# Heading\n\nbody under it\n",
        "- a\n- b\n  - c\n",
        "> quoted\n> more\n",
        "before\n```rust\nfn main() {}\n```\nafter\n",
        // An open fence: the rule above it gains `rust` only when it closes.
        "before\n```rust\nfn main() {\n",
        // Streaming in: nothing is complete yet.
        "half a li",
        "中文段落也要能换行\n第二行\n",
        "```\nno language\n```\n",
        "",
        "\n\n\n",
    ];

    /// The settled render is the one it replaced: same lines, always.
    #[test]
    fn rendering_is_unchanged_by_reporting_where_it_settled() {
        for doc in CORPUS {
            for w in [1u16, 3, 8, 20, 80] {
                assert_eq!(
                    render(doc, w, Style::new()),
                    render_settled(doc, w, Style::new()).lines,
                    "doc {doc:?} at width {w}"
                );
            }
        }
    }

    /// The property a caller resumes on: the settled prefix, rendered on its
    /// own, is exactly the lines the full render put there. If this held only
    /// sometimes, keeping the prefix would paint stale lines.
    #[test]
    fn the_settled_prefix_renders_the_same_on_its_own() {
        for doc in CORPUS {
            for w in [3u16, 8, 20, 80] {
                let r = render_settled(doc, w, Style::new());
                #[allow(
                    clippy::string_slice,
                    reason = "`r.settled` is an offset this renderer produced at a line boundary (the byte after a `\\n`), so it is a char boundary"
                )]
                let prefix = &doc[..r.settled];
                // Truncated, because `render("")` is one empty line while
                // nothing settled is zero lines: an empty prefix has nothing to
                // carry over, and the caller renders the whole text instead.
                let mut prefix_lines = render(prefix, w, Style::new());
                prefix_lines.truncate(r.settled_lines);
                assert_eq!(
                    prefix_lines,
                    r.lines[..r.settled_lines].to_vec(),
                    "prefix {prefix:?} of {doc:?} at width {w}"
                );
            }
        }
    }

    #[test]
    fn an_open_fence_is_never_settled_past_its_opening() {
        // The block is emitted only when the fence closes, and the rule above it
        // then gains the language label — so everything from the opener on is
        // still in flux, and the settled prefix must stop before it.
        let text = "intro\n\n```rust\nfn main() {\n    let x = 1;\n";
        let r = render_settled(text, 40, Style::new());
        #[allow(
            clippy::string_slice,
            reason = "`r.settled` is an offset this renderer produced at a line boundary (the byte after a `\\n`), so it is a char boundary"
        )]
        let prefix = &text[..r.settled];
        assert_eq!(
            prefix, "intro\n\n",
            "settled stops at the last boundary outside the fence"
        );
        // Closing the fence settles the whole block — everything but the empty
        // line after the final newline, which more text could still extend.
        let closed = format!("{text}```\n");
        let r = render_settled(&closed, 40, Style::new());
        assert_eq!(r.settled, closed.len());
        assert_eq!(
            r.settled_lines + 1,
            r.lines.len(),
            "the still-arriving last line is not settled"
        );
    }

    #[test]
    fn nothing_ever_exceeds_the_width_at_any_width() {
        let doc = "# A heading that is quite long indeed\n\n\
                   Some **bold** and *italic* and `inline code` in a paragraph that wraps.\n\n\
                   - a bullet\n- another with a very long body that certainly needs wrapping\n\
                   1. numbered\n\n\
                   > a quotation that also happens to be long enough to wrap somewhere\n\n\
                   ```rust\nfn main() { let x = \"hi\"; }\n```\n\n\
                   中文段落也要能正确换行不能把宽字符劈成两半\n\
                   https://example.com/a/very/long/url/that/cannot/be/broken/at/spaces";
        for w in 1..=100u16 {
            for line in render(doc, w, Style::new()) {
                assert!(
                    line.width() <= w as usize,
                    "width {w}: {:?} is {} cells",
                    line.plain(),
                    line.width()
                );
            }
        }
    }

    #[test]
    fn markers_are_consumed_rather_than_shown() {
        let out = plain("**bold** and *it* and `code`", 80);
        assert_eq!(out[0], "bold and it and code");
    }

    #[test]
    fn a_link_shows_its_text_not_its_url() {
        let out = plain("see [the docs](https://example.com/very/long)", 80);
        assert_eq!(out[0], "see the docs");
    }

    #[test]
    fn a_fenced_block_is_framed_and_labelled() {
        let out = plain("```rust\nfn main() {}\n```", 30);
        assert!(out[0].starts_with("─ rust "), "{out:?}");
        assert_eq!(out[1], "fn main() {}");
        assert!(out[2].chars().all(|c| c == '─'));
    }

    #[test]
    fn an_unterminated_fence_still_shows_what_arrived() {
        // Exactly what a half-streamed answer looks like.
        let out = plain("```rust\nfn main() {", 30);
        assert!(out.iter().any(|l| l.contains("fn main")), "{out:?}");
    }

    #[test]
    fn nothing_inside_a_fence_is_treated_as_markdown() {
        let out = plain("```\n# not a heading\n- not a bullet\n```", 40);
        assert!(out.iter().any(|l| l.contains("# not a heading")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("- not a bullet")), "{out:?}");
    }

    #[test]
    fn headings_and_lists_get_their_own_markers() {
        let out = plain("## Title\n- one\n2. two", 40);
        assert_eq!(out[0], "## Title");
        assert_eq!(out[1], "• one");
        assert_eq!(out[2], "2. two");
    }

    #[test]
    fn keywords_and_strings_are_lit_but_the_text_is_untouched() {
        let lines = render("```rust\nlet s = \"hi\";\n```", 40, Style::new());
        let code = &lines[1];
        assert_eq!(code.plain(), "let s = \"hi\";");
        assert!(
            code.spans.len() > 1,
            "it should be several styled runs, not one"
        );
        assert!(code
            .spans
            .iter()
            .any(|s| s.style.fg == Some(Color::role(Role::Brand))));
    }

    #[test]
    fn an_unclosed_marker_is_text_not_a_swallowed_rest_of_line() {
        assert_eq!(plain("a * b", 40)[0], "a * b");
        assert_eq!(plain("unclosed `code", 40)[0], "unclosed `code");
    }

    #[test]
    fn rendering_is_total_and_terminates_on_anything() {
        for doc in [
            "",
            "`",
            "```",
            "***",
            "[](",
            "####### too many",
            "\u{0}\u{1}",
            &"*".repeat(200),
            &"中".repeat(200),
        ] {
            for w in [0u16, 1, 2, 3, 80] {
                let _ = render(doc, w, Style::new());
            }
        }
    }
}
