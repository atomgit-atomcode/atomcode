//! Markdown, rendered into styled lines.
//!
//! Not a port of the existing renderer: that one produces ANSI strings a line
//! at a time, and this architecture needs structured spans. The difference is
//! not cosmetic — spans are what let the host check that nothing drew outside
//! its rect, and a pre-baked escape sequence cannot be measured or re-wrapped.
//!
//! Deliberately partial. It covers what a coding conversation actually
//! contains — fenced code, inline code, emphasis, headings, lists, quotes,
//! links, and GFM tables ([`table`]) — and stops there. Footnotes, HTML and
//! setext headings are not rendered specially; they come out as text, which is
//! honest and readable, rather than half-supported.

use crate::frame::{Color, Line, Span, Style};
use crate::theme::Role;
use crate::width;

mod table;

/// Markdown's one decoration: the rule, under a heading and around a fenced
/// block, and under a table's header.
///
/// Named once, and for a reason beyond tidiness: `caps::downgrade` rewrites it
/// to `-` at paint time on a terminal that cannot show it, so what matters is
/// that every one of them is the *same* character. (The layering ratchet counts
/// this glyph per occurrence in the source, which is the other half of the same
/// argument — one site, one decision.)
pub(super) const RULE: &str = "─";

/// The left gutter bar of a fenced code block. A thin block, not a box vertical:
/// it reads as a coloured margin beside the code rather than another drawn line,
/// which is the whole point of dropping the rules. `caps::ascii_for` downgrades it
/// to `|` where Unicode is unavailable.
pub(super) const CODE_BAR: &str = "▏";

fn code() -> Style {
    // Inline code is a COOL accent, not a warning colour. Highlighting every
    // identifier / path / command in `Role::Warning` (orange) made a normal
    // paragraph loud and busy; the calmer peers tint code in the accent hue and
    // keep the loud/warning colours for actual warnings. Distinguished from a
    // heading by weight — heading is the same accent but BOLD.
    Style::new().fg(Color::role(Role::Accent))
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
/// fence and outside any table block. A caller may keep `lines[..settled_lines]`
/// and render only the rest next time, because appending source lines can change
/// nothing before the last boundary that was outside both.
///
/// Two things are *not* settled, wherever they start:
///
/// * an **open fence** — its body is emitted only when the fence closes, and the
///   rule above it then gains the language label, both retroactively;
/// * a **buffered table block** — a row is only known to be a table row once a
///   delimiter row shows up, and every row's arrival can change every column's
///   width, so the whole block is re-laid-out at once.
///
/// So `settled` stays before the opener, and before the first buffered row,
/// until each is resolved.
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
    // Rows of a table block that has not ended yet. See `settled` above.
    let mut table_rows: Vec<String> = Vec::new();
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
        let body = trimmed.trim_start();
        let indent = trimmed.len() - body.len();
        // Fences first: inside a block, nothing else is markdown.
        if let Some(rest) = body.strip_prefix("```") {
            flush_table(&mut table_rows, &mut out, w, base);
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
            match table_row(body) {
                Some(row) => table_rows.push(row),
                None => {
                    flush_table(&mut table_rows, &mut out, w, base);
                    out.extend(ordinary_line(body, indent, w, base));
                }
            }
        }
        off = if complete { line_end + 1 } else { line_end };
        if complete && in_code.is_none() && table_rows.is_empty() {
            settled = off;
            settled_lines = out.len();
        }
    }
    // An unterminated fence is common in a stream that is still arriving; show
    // what there is rather than swallowing it. A table block that never got its
    // closing blank line is the same case.
    if in_code.is_some() && !code_lines.is_empty() {
        out.extend(code_block(&code_lines, "", w));
    }
    flush_table(&mut table_rows, &mut out, w, base);
    Rendered {
        lines: out,
        settled,
        settled_lines,
    }
}

/// One line as a table row — its canonical `|` form — or `None` when it cannot
/// be one.
///
/// Deliberately broad: any line that splits into two or more cells is buffered,
/// because GFM tables need not have leading or trailing `|` and weak models emit
/// both forms. What it must *not* swallow is a line that already has a meaning
/// of its own, so everything this renderer treats as block structure — a
/// heading, a rule, a quote, a list item — is excluded here. A bullet that
/// merely mentions a pipe (`- option A | option B`) is a list item, and losing
/// its marker to a table that then turns out not to exist is the bug that guard
/// comes from.
fn table_row(t: &str) -> Option<String> {
    if t.is_empty()
        || heading_of(t).is_some()
        || is_rule(t)
        || t.starts_with('>')
        || list_item(t).is_some()
    {
        return None;
    }
    table::row(t)
}

/// Emit a buffered table block — or hand it back as ordinary lines.
///
/// A block of pipe-splitting lines is only a table when a delimiter row
/// (`---|---`) is among them; a paragraph that happens to contain a `|` is
/// prose, and drawing a box around it would invent structure that is not there.
fn flush_table(rows: &mut Vec<String>, out: &mut Vec<Line>, w: u16, base: Style) {
    let buffered = std::mem::take(rows);
    if buffered.is_empty() {
        return;
    }
    match table::render(&buffered, w, base) {
        Some(lines) => out.extend(lines),
        None => {
            for row in &buffered {
                out.extend(ordinary_line(row.trim_start(), 0, w, base));
            }
        }
    }
}

/// One line that is not part of a table: a blank, a heading, a rule, a quote, a
/// list item, or prose.
fn ordinary_line(t: &str, indent: usize, w: u16, base: Style) -> Vec<Line> {
    if t.is_empty() {
        return vec![Line::empty()];
    }
    if let Some((_level, title)) = heading_of(t) {
        // The `#` markers are the source's, not the reader's: a rendered heading
        // is bold and accent-coloured, it does not carry its own `##`. The level
        // is not drawn apart — bold accent is the whole cue — so `## x` and
        // `### x` read alike, which is the price of not showing the hashes.
        return wrap_spans(&inline(title, heading()), w, "", heading());
    }
    if is_rule(t) {
        return vec![Line::styled(RULE.repeat(w as usize), fence())];
    }
    if let Some(b) = t.strip_prefix("> ").or_else(|| t.strip_prefix(">")) {
        // A blockquote is a CALLOUT, not a heading. Keep the Accent gutter to mark it,
        // but render the quoted TEXT as body (`base`, like an ordinary paragraph) — the
        // text sharing `quote()`'s Accent with `heading()` made a `>` line read as a
        // heading and lowered readability on a full sentence. Inline code still highlights.
        return wrap_spans(&inline(b, base), w, "▏ ", quote());
    }
    if let Some((marker, b)) = list_item(t) {
        let lead = format!("{}{marker} ", " ".repeat(indent));
        // An ordered marker (`1.`) is drawn in the body's own colour, not the
        // bullet accent: a number is content a reader counts, not chrome, and the
        // accent made it read like a link. The unordered `•` keeps the accent —
        // a coloured dot is what makes an unnumbered list read as a list.
        let marker_style = if marker.starts_with(|c: char| c.is_ascii_digit()) {
            base
        } else {
            bullet()
        };
        return wrap_spans(&inline(b, base), w, &lead, marker_style);
    }
    wrap_spans(&inline(t, base), w, " ".repeat(indent).as_str(), base)
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

/// The URL schemes a terminal can open on click, so a link becomes an OSC 8
/// hyperlink. `mailto:`/relative/anchor destinations stay styled-but-inert —
/// this tree does not send them anywhere.
const OPENABLE_SCHEMES: [&str; 3] = ["https://", "http://", "file://"];

/// True when `url` is a destination a terminal can open (see [`OPENABLE_SCHEMES`]).
fn openable(url: &str) -> bool {
    OPENABLE_SCHEMES
        .iter()
        .any(|scheme| url.len() > scheme.len() && url.starts_with(scheme))
}

/// File extensions a bare ABSOLUTE path must end in to become a `file://` link —
/// pages and media a terminal can hand to a browser or default viewer. A path
/// without one of these (a source file, a directory) is left as plain text: this
/// affordance is for "open the thing you just generated", not every path.
const OPENABLE_EXTENSIONS: &[&str] = &[
    "html", "htm", "pdf", "svg", "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "ico", "mp4",
    "webm",
];

/// True when `path` is an absolute path ending in an [`OPENABLE_EXTENSIONS`]
/// name — the only local paths this tree turns into `file://` links, so a bare
/// `/etc/hosts` or `/usr/bin` never becomes one.
fn path_openable(path: &str) -> bool {
    // Absolute, and NOT protocol-relative (`//cdn/x.png`): a leading `//` is a
    // network address, not a local file, and `file://`-prefixing it would make a
    // malformed `file:////…` authority.
    if !path.starts_with('/') || path.starts_with("//") {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or("");
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => OPENABLE_EXTENSIONS
            .iter()
            .any(|e| ext.eq_ignore_ascii_case(e)),
        _ => false,
    }
}

/// Push `text` as spans, pulling any bare openable link (a web/`file://` URL or
/// an absolute openable path) into its own clickable run — styled with [`link`],
/// carrying the OSC 8 target — and leaving the rest as `base` text.
fn push_linkified(text: &str, base: Style, out: &mut Vec<Span>) {
    let mut rest = text;
    while let Some((start, len, target)) = next_link(rest) {
        if start > 0 {
            #[allow(
                clippy::string_slice,
                reason = "`start` is a char-boundary byte offset from `next_link`"
            )]
            out.push(Span::styled(rest[..start].to_string(), base));
        }
        #[allow(
            clippy::string_slice,
            reason = "`start`/`len` are char-boundary byte offsets from `next_link`"
        )]
        let shown = rest[start..start + len].to_string();
        // The shown text is the link as written; the OSC 8 target is the URL it
        // stands for (itself for a URL, `file://`+path for an absolute path).
        out.push(Span::linked(shown, link(), target));
        #[allow(
            clippy::string_slice,
            reason = "`start + len` is a char boundary — the link's end from `next_link`"
        )]
        {
            rest = &rest[start + len..];
        }
    }
    if !rest.is_empty() {
        out.push(Span::styled(rest.to_string(), base));
    }
}

/// The first bare link in `s` — a scheme URL or an absolute openable path,
/// whichever comes first — as `(byte offset, byte length, osc8_target)`.
///
/// The shown text is `s[offset..offset+len]`; the OSC 8 target is the URL itself
/// for a scheme link, or `file://` prepended for a path. A `file://` URL wins
/// over the path inside it (it starts earlier), so the two never double-link.
fn next_link(s: &str) -> Option<(usize, usize, String)> {
    let url = next_url(s);
    let path = next_path(s);
    #[allow(
        clippy::string_slice,
        reason = "offsets/lengths from next_url/next_path are char boundaries"
    )]
    match (url, path) {
        (Some((us, ul)), Some((ps, _))) if us <= ps => Some((us, ul, s[us..us + ul].to_string())),
        (_, Some((ps, pl))) => Some((ps, pl, format!("file://{}", &s[ps..ps + pl]))),
        (Some((us, ul)), None) => Some((us, ul, s[us..us + ul].to_string())),
        (None, None) => None,
    }
}

/// The first bare absolute openable path in `s`, as `(byte offset, byte length)`.
///
/// A path starts at a `/` sitting at a word boundary (start of string, or after
/// whitespace/an opener) so `and/or.html` or a `//` mid-token is not mistaken for
/// one, runs to the first whitespace/control, has trailing sentence punctuation
/// trimmed, and must end in an [`OPENABLE_EXTENSIONS`] name. `None` otherwise.
fn next_path(s: &str) -> Option<(usize, usize)> {
    let mut boundary = true;
    for (i, c) in s.char_indices() {
        if c == '/' && boundary {
            #[allow(
                clippy::string_slice,
                reason = "`i` is a byte offset from `char_indices`, a char boundary"
            )]
            let tail = &s[i..];
            let end = tail
                .char_indices()
                .find(|(_, c)| c.is_whitespace() || c.is_control())
                .map(|(j, _)| j)
                .unwrap_or(tail.len());
            #[allow(
                clippy::string_slice,
                reason = "`end` is a char boundary from `char_indices`"
            )]
            let trimmed = tail[..end].trim_end_matches(|c: char| {
                matches!(
                    c,
                    '.' | ','
                        | ';'
                        | ':'
                        | '!'
                        | '?'
                        | ')'
                        | ']'
                        | '}'
                        | '>'
                        | '"'
                        | '\''
                        | '，'
                        | '。'
                        | '、'
                        | '」'
                        | '』'
                        | '）'
                )
            });
            if path_openable(trimmed) {
                return Some((i, trimmed.len()));
            }
        }
        boundary = c.is_whitespace()
            || matches!(
                c,
                '(' | '[' | '{' | '<' | '"' | '\'' | '（' | '「' | '『' | '【' | '《'
            );
    }
    None
}

/// The first bare openable URL in `s`, as `(byte offset, byte length)`.
///
/// A URL runs from its scheme to the first whitespace or control char, then has
/// trailing sentence punctuation trimmed (`https://x/issues/5).` → the URL, not
/// the `).`). A rare URL that genuinely ends in one of those characters loses
/// it — the accepted cost of not needing a full grammar. `None` when nothing but
/// a bare scheme is present (`https://` with no host is not a link).
fn next_url(s: &str) -> Option<(usize, usize)> {
    // Scan forward: a scheme that trims to a non-openable candidate (a bare
    // `https://` with no host) must not hide a real URL later in the same run, so
    // a dud advances the cursor rather than ending the search.
    let mut cursor = 0;
    while cursor < s.len() {
        #[allow(
            clippy::string_slice,
            reason = "`cursor` is 0 or one past an ASCII scheme byte — a char boundary"
        )]
        let ahead = &s[cursor..];
        let start = OPENABLE_SCHEMES
            .iter()
            .filter_map(|scheme| ahead.find(scheme).map(|i| cursor + i))
            .min()?;
        #[allow(
            clippy::string_slice,
            reason = "`start` is a byte offset from `find`, always a char boundary"
        )]
        let tail = &s[start..];
        let end = tail
            .char_indices()
            .find(|(_, c)| c.is_whitespace() || c.is_control())
            .map(|(i, _)| i)
            .unwrap_or(tail.len());
        #[allow(
            clippy::string_slice,
            reason = "`end` is a char boundary from `char_indices`"
        )]
        let trimmed = tail[..end].trim_end_matches(|c: char| {
            matches!(
                c,
                '.' | ','
                    | ';'
                    | ':'
                    | '!'
                    | '?'
                    | ')'
                    | ']'
                    | '}'
                    | '>'
                    | '"'
                    | '\''
                    | '，'
                    | '。'
                    | '、'
                    | '」'
                    | '』'
                    | '）'
            )
        });
        if openable(trimmed) {
            return Some((start, trimmed.len()));
        }
        // Past this scheme's first byte (ASCII, so a boundary) to find the next.
        cursor = start + 1;
    }
    None
}

/// Split one line into styled runs: `code`, **bold**, *italic*, [text](url), and
/// bare `http(s)://` / `file://` URLs.
fn inline(text: &str, base: Style) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;

    let flush = |buf: &mut String, out: &mut Vec<Span>| {
        if !buf.is_empty() {
            // Bare `http(s)://` / `file://` runs are pulled out of plain text into
            // clickable link spans; everything else stays `base` text.
            push_linkified(&std::mem::take(buf), base, out);
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
                            let url: String = chars[close + 2..paren].iter().collect();
                            // The label is always styled as a link; it becomes a
                            // clickable OSC 8 hyperlink only when the destination is
                            // one a terminal can open (a relative/anchor markdown
                            // link stays styled-but-inert).
                            out.push(if openable(&url) {
                                Span::linked(label, link(), url)
                            } else {
                                Span::styled(label, link())
                            });
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
///
/// Shared with the blocks that wrap their own head rather than cut it — a tool
/// call's expanded line shows the command whole, and this is the same wrapping
/// the prose goes through.
pub(crate) fn wrap_spans(spans: &[Span], w: u16, prefix: &str, prefix_style: Style) -> Vec<Line> {
    let indent = width::str_width(prefix);
    let body = (w as usize).saturating_sub(indent).max(1);
    let mut out: Vec<Line> = Vec::new();
    let mut current = Line::from_spans(vec![Span::styled(prefix.to_string(), prefix_style)]);
    let mut used = 0usize;

    for span in spans {
        // A newline in the source is a break in the output, not a byte of a
        // word. A `Line` that keeps one is written by the terminal as extra rows
        // the scroll is not counting, so everything under it is drawn on top of
        // what it covered — which is what a command written over several rows
        // (a heredoc) used to do to the transcript.
        for (i, segment) in span.text.split('\n').enumerate() {
            let segment = segment.strip_suffix('\r').unwrap_or(segment);
            if i > 0 {
                out.push(std::mem::replace(
                    &mut current,
                    Line::from_spans(vec![Span::styled(" ".repeat(indent), prefix_style)]),
                ));
                used = 0;
            }
            for word in segment.split_inclusive(' ') {
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
                                Line::from_spans(vec![Span::styled(
                                    " ".repeat(indent),
                                    prefix_style,
                                )]),
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
                        current.push(span.recut(piece));
                    }
                } else {
                    current.push(span.recut(word.to_string()));
                    used += ww;
                }
            }
        }
    }
    out.push(current);
    out.into_iter().map(|l| l.truncate(w as usize)).collect()
}

/// A fenced block: a dim left gutter bar down the side, the language named in the
/// accent colour, the source with keywords lit. No horizontal rules — the bar IS
/// the frame, which is far less chrome than a top-and-bottom rule when an answer
/// stacks several blocks (the "横线太多" complaint). The peers that keep command
/// output light do the same: a single left bar, never a box.
fn code_block(lines: &[String], lang: &str, w: u16) -> Vec<Line> {
    // `▏ ` — a bar and a space, in the muted fence colour. Downgrades to `| ` on a
    // terminal without Unicode (see `caps::ascii_for`), exactly as `RULE` does.
    let bar = || Span::styled(format!("{CODE_BAR} "), fence());
    let mut out = Vec::new();
    if !lang.is_empty() {
        out.push(
            Line::from_spans(vec![bar(), Span::styled(lang.to_string(), code())])
                .truncate(w as usize),
        );
    }
    for line in lines {
        let mut spans = vec![bar()];
        spans.extend(highlight(line, lang));
        out.push(Line::from_spans(spans).truncate(w as usize));
    }
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
    // Comments are prose someone will read, not chrome: `Border` was the
    // dimmest role at the time, and that is the trouble — dimness was never
    // the job here. `Muted` keeps the recede but holds its own floor.
    let comment = Style::new().fg(Color::role(Role::Muted)).italic();
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
        // Tables, in the three states they arrive in: whole, with the delimiter
        // row still missing, and still missing rows after the delimiter.
        "| a | b |\n|---|---|\n| 1 | 2 |\n",
        "para\n\n| 名称 | 说明 |\n|---|---|\n| 中文 | x |\n\nafter\n",
        "| a | b |\n| 1 | 2 |\n",
        "before\n\n| a | b |\n|---|---|\n| 1 | 2 |",
        "┌───┬───┐\n│ a │ b │\n└───┴───┘\n",
        // A pipe that is not a table, next to one that is.
        "see a | b\n| a | b |\n|---|---|\n",
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

    /// The other half of the resume contract, and the half `LiveCache` actually
    /// relies on: the source from `settled` on, rendered on its own, is exactly
    /// the lines the full render put after the prefix. If a table block leaked
    /// across that boundary, the resumed render would draw it twice.
    #[test]
    fn the_tail_past_settled_renders_the_rest_of_the_lines() {
        for doc in CORPUS {
            for w in [3u16, 8, 20, 80] {
                let r = render_settled(doc, w, Style::new());
                #[allow(
                    clippy::string_slice,
                    reason = "`r.settled` is an offset this renderer produced at a line boundary (the byte after a `\\n`), so it is a char boundary"
                )]
                let tail = &doc[r.settled..];
                assert_eq!(
                    render(tail, w, Style::new()),
                    r.lines[r.settled_lines..].to_vec(),
                    "tail {tail:?} of {doc:?} at width {w}"
                );
            }
        }
    }

    #[test]
    fn a_buffered_table_is_never_settled_past_its_first_row() {
        // The delimiter row may still be arriving, and it decides whether this is
        // a table at all — so nothing from the first row on is settled.
        let text = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let r = render_settled(text, 40, Style::new());
        #[allow(
            clippy::string_slice,
            reason = "`r.settled` is an offset this renderer produced at a line boundary (the byte after a `\\n`), so it is a char boundary"
        )]
        let prefix = &text[..r.settled];
        assert_eq!(
            prefix, "intro\n\n",
            "settled stops before the first buffered row"
        );
        // A line that ends the block resolves it, and everything up to the
        // still-arriving last line settles again.
        let closed = format!("{text}\n");
        let r = render_settled(&closed, 40, Style::new());
        assert_eq!(r.settled, closed.len());
        assert_eq!(r.settled_lines + 1, r.lines.len());
    }

    #[test]
    fn a_table_at_the_end_of_a_stream_is_still_drawn() {
        // The block never got its closing blank line: no row may be swallowed.
        let out = plain("text\n\n| 名称 | 说明 |\n|---|---|\n| 中文 | x |", 40);
        assert!(
            out.iter().all(|l| !l.contains('|')),
            "the pipes leaked into the output: {out:?}"
        );
        assert!(
            out.iter().any(|l| l.contains("中文") && l.contains('x')),
            "the table was not drawn: {out:?}"
        );
    }

    #[test]
    fn a_single_column_table_is_drawn_rather_than_left_as_pipes() {
        // A one-column table is still a table: it arrived here as the literal
        // pipes it was written with, because every row of it splits into a
        // single cell.
        let out = plain("| 只有一列 |\n|:---|\n| 单列也要画出来 |", 40);
        assert!(
            out.iter().all(|l| !l.contains('|')),
            "the pipes leaked into the output: {out:?}"
        );
        assert!(
            out.iter().any(|l| l.contains("单列也要画出来")),
            "the table was not drawn: {out:?}"
        );
        assert!(
            // Drawn inside a box: a top border opens it.
            out.iter().any(|l| l.starts_with('┌')),
            "the table was not boxed: {out:?}"
        );
    }

    #[test]
    fn a_list_item_that_mentions_a_pipe_keeps_its_bullet() {
        let out = plain("- option A | option B\n1. run a | run b", 40);
        assert_eq!(out[0], "• option A | option B");
        assert_eq!(out[1], "1. run a | run b");
    }

    #[test]
    fn prose_with_a_pipe_comes_out_as_prose() {
        // Broad detection buffers this; no delimiter row means it was never a
        // table, and it must come back out looking like what was written.
        assert_eq!(
            plain("see a | b in the docs", 40)[0],
            "see a | b in the docs"
        );
        let out = plain("use a | b\nor c | d", 40);
        assert_eq!(out[0], "use a | b");
        assert_eq!(out[1], "or c | d");
    }

    #[test]
    fn a_heading_or_quote_that_mentions_a_pipe_is_not_a_table_row() {
        let out = plain("## a | b\n> c | d", 40);
        // A heading, drawn without its `##` marker — not a table row.
        assert_eq!(out[0], "a | b");
        assert_eq!(out[1], "▏ c | d");
    }

    #[test]
    fn a_box_drawing_table_arriving_as_text_is_drawn_as_a_table() {
        let out = plain(
            "┌──────┬──────┐\n│ a    │ b    │\n├──────┼──────┤\n│ 1    │ 2    │\n└──────┴──────┘",
            40,
        );
        // A pre-drawn table is re-rendered as this table's own box — not passed
        // through — so the ASCII shapes of the source never leak.
        assert!(
            !out.iter().any(|l| l.contains('|') || l.contains("---")),
            "the source's ASCII pipes leaked: {out:?}"
        );
        // The columns survive, inside our box (a content row carries both).
        assert!(
            out.iter()
                .any(|l| l.starts_with('│') && l.contains('a') && l.contains('b')),
            "the columns were lost: {out:?}"
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

    /// The spans a run of text produced, flattened across the wrapped lines.
    fn spans_of(text: &str, width: u16) -> Vec<Span> {
        render(text, width, Style::new())
            .into_iter()
            .flat_map(|l| l.spans)
            .collect()
    }

    #[test]
    fn a_bare_url_becomes_a_clickable_link_run() {
        // The model's own example: a bare issue URL in prose is pulled out into a
        // span that both reads as a link (styled) and carries the URL for OSC 8.
        let spans = spans_of("see https://atomgit.com/x/atomcode/issues/1565 now", 200);
        let link = spans
            .iter()
            .find(|s| s.link.is_some())
            .expect("a linked run");
        assert_eq!(link.text, "https://atomgit.com/x/atomcode/issues/1565");
        assert_eq!(
            link.link.as_deref(),
            Some("https://atomgit.com/x/atomcode/issues/1565")
        );
        // The prose around it stays ordinary text, not swallowed into the link.
        let prose: String = spans
            .iter()
            .filter(|s| s.link.is_none())
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            prose.contains("see") && prose.contains("now"),
            "prose: {prose:?}"
        );
    }

    #[test]
    fn trailing_sentence_punctuation_stays_out_of_the_url() {
        // `(https://…/1565).` — the closing paren and period are the sentence's,
        // not the URL's.
        let spans = spans_of("(https://atomgit.com/x/issues/1565).", 200);
        let link = spans
            .iter()
            .find(|s| s.link.is_some())
            .expect("a linked run");
        assert_eq!(
            link.link.as_deref(),
            Some("https://atomgit.com/x/issues/1565")
        );
    }

    #[test]
    fn a_file_url_is_a_link_a_bare_scheme_is_not() {
        let spans = spans_of("open file:///tmp/report.html or just file://", 200);
        let links: Vec<&str> = spans.iter().filter_map(|s| s.link.as_deref()).collect();
        assert_eq!(links, ["file:///tmp/report.html"], "{spans:?}");
    }

    #[test]
    fn a_bare_scheme_does_not_hide_a_later_real_url() {
        // A dud first candidate (`https://` with no host) must not end the scan:
        // the real URL after it is still found.
        let spans = spans_of("the scheme is https:// e.g. https://real.example/x", 200);
        let links: Vec<&str> = spans.iter().filter_map(|s| s.link.as_deref()).collect();
        assert_eq!(links, ["https://real.example/x"], "{spans:?}");
    }

    #[test]
    fn a_markdown_link_carries_its_url_when_openable_and_not_otherwise() {
        let openable = spans_of("see [docs](https://example.com/p)", 200);
        let a = openable
            .iter()
            .find(|s| s.text == "docs")
            .expect("the label");
        assert_eq!(a.link.as_deref(), Some("https://example.com/p"));

        // A relative destination stays styled-as-a-link but inert (nowhere to open).
        let relative = spans_of("see [guide](./guide.md)", 200);
        let b = relative
            .iter()
            .find(|s| s.text == "guide")
            .expect("the label");
        assert!(
            b.link.is_none(),
            "a relative link is not an OSC 8 hyperlink"
        );
    }

    #[test]
    fn a_generated_absolute_path_becomes_a_file_link() {
        // The screenshot case: the agent reports where it wrote a page, and that
        // bare absolute path is clickable — shown as written, opened as `file://`.
        let spans = spans_of(
            "已在默认浏览器打开 /Users/theo/Documents/workspace/atomcode/pelican-bike.html。",
            300,
        );
        let link = spans
            .iter()
            .find(|s| s.link.is_some())
            .expect("a linked run");
        assert_eq!(
            link.text,
            "/Users/theo/Documents/workspace/atomcode/pelican-bike.html"
        );
        assert_eq!(
            link.link.as_deref(),
            Some("file:///Users/theo/Documents/workspace/atomcode/pelican-bike.html")
        );
    }

    #[test]
    fn a_path_without_an_openable_extension_stays_plain() {
        // A source file or a directory is not something to open in a browser.
        for text in [
            "see /etc/hosts here",
            "cd /usr/local/bin now",
            "edit /src/main.rs",
        ] {
            let spans = spans_of(text, 200);
            assert!(
                spans.iter().all(|s| s.link.is_none()),
                "no link for {text:?}: {spans:?}"
            );
        }
    }

    #[test]
    fn a_slash_mid_word_is_not_a_path() {
        // `and/or.html` / a relative `docs/x.png` is not an absolute path — the
        // leading slash must sit at a word boundary.
        for text in ["pick and/or.html today", "at docs/guide.png ok"] {
            let spans = spans_of(text, 200);
            assert!(
                spans.iter().all(|s| s.link.is_none()),
                "no link for {text:?}: {spans:?}"
            );
        }
    }

    #[test]
    fn a_protocol_relative_double_slash_is_not_a_file_path() {
        // `//cdn/logo.png` is a network address, not a local file — it must not
        // become a malformed `file:////…` link.
        let spans = spans_of("logo at //cdn.example.com/logo.png here", 200);
        assert!(spans.iter().all(|s| s.link.is_none()), "{spans:?}");
    }

    #[test]
    fn a_file_url_does_not_double_link_its_inner_path() {
        // `file:///a/b.html` is one link (the URL), not the URL plus the `/a/b.html`
        // inside it.
        let spans = spans_of("open file:///a/b.html", 200);
        let links: Vec<&str> = spans.iter().filter_map(|s| s.link.as_deref()).collect();
        assert_eq!(links, ["file:///a/b.html"], "{spans:?}");
    }

    #[test]
    fn a_fenced_block_uses_a_gutter_bar_not_rules() {
        let out = plain("```rust\nfn main() {}\n```", 30);
        // A label line and the code, each down a left bar — and NO horizontal
        // rules: the bar is the whole frame.
        assert_eq!(out.len(), 2, "no top/bottom rules: {out:?}");
        assert_eq!(out[0], "▏ rust");
        assert_eq!(out[1], "▏ fn main() {}");
        assert!(
            !out.iter().any(|l| l.chars().all(|c| c == '─')),
            "no line is a horizontal rule: {out:?}"
        );
    }

    #[test]
    fn the_fence_language_is_coloured_and_the_bar_is_dim() {
        let out = render("```bash\nls\n```", 30, Style::new());
        let head = &out[0];
        // The bar is drawn in the muted fence colour…
        assert_eq!(
            head.spans[0].style.fg,
            Some(Color::role(Role::Muted)),
            "dim bar: {head:?}"
        );
        // …and the language is named in the accent colour so it reads at a glance.
        assert!(
            head.spans
                .iter()
                .any(|s| s.text.trim() == "bash" && s.style.fg == Some(Color::role(Role::Accent))),
            "accent language label: {head:?}"
        );
        // Every code row also starts with the dim gutter bar.
        let code = &out[1];
        assert!(code.spans[0].text.starts_with('▏'), "{code:?}");
        assert_eq!(code.spans[0].style.fg, Some(Color::role(Role::Muted)));
    }

    #[test]
    fn a_fence_without_a_language_has_a_bar_but_no_label() {
        let out = plain("```\nraw line\n```", 30);
        // No language ⇒ no label line, just the barred code.
        assert_eq!(out, vec!["▏ raw line".to_string()]);
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
    fn headings_lose_their_hashes_and_lists_keep_their_markers() {
        let out = plain("## Title\n- one\n2. two", 40);
        // The heading is drawn without its `##`; the list markers stay.
        assert_eq!(out[0], "Title");
        assert_eq!(out[1], "• one");
        assert_eq!(out[2], "2. two");
    }

    #[test]
    fn an_ordered_marker_is_plain_while_a_bullet_keeps_its_accent() {
        let lines = render("- one\n2. two", 40, Style::new());
        // The unordered `•` is drawn in the bullet accent (`Role::Border`) — a
        // coloured dot is what makes an unnumbered list read as a list.
        let dot = lines[0]
            .spans
            .iter()
            .find(|s| s.text.contains('•'))
            .expect("a bullet marker");
        assert_eq!(dot.style.fg, Some(Color::role(Role::Border)), "{dot:?}");
        // The ordered `2.` is the body's own colour — no highlight, no accent.
        let num = lines[1]
            .spans
            .iter()
            .find(|s| s.text.contains("2."))
            .expect("a number marker");
        assert_eq!(num.style.fg, None, "the number should be plain: {num:?}");
    }

    #[test]
    fn keywords_and_strings_are_lit_but_the_text_is_untouched() {
        let lines = render("```rust\nlet s = \"hi\";\n```", 40, Style::new());
        let code = &lines[1];
        // The code sits after the gutter bar, unchanged.
        assert_eq!(code.plain(), "▏ let s = \"hi\";");
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
