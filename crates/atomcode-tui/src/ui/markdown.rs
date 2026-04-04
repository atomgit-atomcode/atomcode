use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, HeadingLevel};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use super::theme;

pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    static RENDERER: std::sync::OnceLock<MarkdownRenderer> = std::sync::OnceLock::new();
    let renderer = RENDERER.get_or_init(MarkdownRenderer::new);
    let cleaned = strip_emoji(input);
    let spaced = pangu_spacing(&cleaned);
    renderer.render(&spaced)
}

/// Pangu spacing: add a space between CJK and ASCII characters.
/// "已完成WritingView.vue的修改" → "已完成 WritingView.vue 的修改"
fn pangu_spacing(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + s.len() / 4);
    let chars: Vec<char> = s.chars().collect();

    for i in 0..chars.len() {
        result.push(chars[i]);

        if i + 1 < chars.len() {
            let cur_cjk = is_cjk_ideograph(chars[i]);
            let next_cjk = is_cjk_ideograph(chars[i + 1]);
            let cur_ascii = chars[i].is_ascii_alphanumeric();
            let next_ascii = chars[i + 1].is_ascii_alphanumeric();

            if (cur_cjk && next_ascii) || (cur_ascii && next_cjk) {
                result.push(' ');
            }
        }
    }
    result
}

fn is_cjk_ideograph(c: char) -> bool {
    let cp = c as u32;
    (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp)
}


fn strip_emoji(s: &str) -> String {
    s.chars().filter(|c| {
        let cp = *c as u32;
        !((0x1F600..=0x1F64F).contains(&cp) ||   // Emoticons
          (0x1F300..=0x1F5FF).contains(&cp) ||    // Misc Symbols & Pictographs
          (0x1F680..=0x1F6FF).contains(&cp) ||    // Transport & Map
          (0x1F1E0..=0x1F1FF).contains(&cp) ||    // Flags
          (0x1F900..=0x1F9FF).contains(&cp) ||    // Supplemental Symbols
          (0x1FA00..=0x1FA6F).contains(&cp) ||    // Chess Symbols
          (0x1FA70..=0x1FAFF).contains(&cp) ||    // Symbols Extended-A
          (0x2700..=0x27BF).contains(&cp) ||       // Dingbats (✅❌⚡ etc.)
          (0x2600..=0x26FF).contains(&cp))         // Misc Symbols (⚠️☁️ etc.)
    }).collect()
}

struct MarkdownRenderer {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl MarkdownRenderer {
    fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set.themes["base16-ocean.dark"].clone();
        Self { syntax_set, theme }
    }

    fn render(&self, input: &str) -> Vec<Line<'static>> {
        let options = Options::all();
        let parser = Parser::new_ext(input, options);

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut styles: Vec<Style> = vec![Style::default().fg(theme::text_secondary())];
        let mut in_code = false;
        let mut code_lang = String::new();
        let mut code_buf = String::new();
        let mut link_url: Option<String> = None;
        let mut list_depth: usize = 0;
        let mut ordered_idx: Option<u64> = None;
        let mut in_quote = false;
        let mut table_header: Vec<String> = Vec::new();
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut table_row: Vec<String> = Vec::new();
        let mut collecting_header = false;

        for event in parser {
            match event {
                // Headings — only add blank line before H1/H2, not H3+
                Event::Start(Tag::Heading { level, .. }) => {
                    if !lines.is_empty() && matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                        lines.push(Line::default());
                    }
                    let color = match level {
                        HeadingLevel::H1 => theme::md_h1(),
                        HeadingLevel::H2 => theme::md_h2(),
                        _ => theme::md_h3(),
                    };
                    styles.push(Style::default().fg(color).add_modifier(Modifier::BOLD));
                }
                Event::End(TagEnd::Heading(_level)) => {
                    styles.pop();
                    flush(&mut lines, &mut spans);
                }

                // Inline styles
                Event::Start(Tag::Strong) => {
                    let s = cur_style(&styles).fg(theme::text_primary()).add_modifier(Modifier::BOLD);
                    styles.push(s);
                }
                Event::End(TagEnd::Strong) => { styles.pop(); }
                Event::Start(Tag::Emphasis) => {
                    let s = cur_style(&styles).add_modifier(Modifier::ITALIC);
                    styles.push(s);
                }
                Event::End(TagEnd::Emphasis) => { styles.pop(); }

                // Links
                Event::Start(Tag::Link { dest_url, .. }) => {
                    styles.push(Style::default().fg(theme::md_link()).add_modifier(Modifier::UNDERLINED));
                    link_url = Some(dest_url.to_string());
                }
                Event::End(TagEnd::Link) => {
                    styles.pop();
                    if let Some(url) = link_url.take() {
                        let last = spans.last().map(|s| s.content.to_string()).unwrap_or_default();
                        if !last.is_empty() && last != url && !url.is_empty() {
                            spans.push(Span::styled(format!(" ({})", url), Style::default().fg(theme::text_muted())));
                        }
                    }
                }

                // Code blocks — clean, no border decorations
                Event::Start(Tag::CodeBlock(kind)) => {
                    in_code = true;
                    code_buf.clear();
                    code_lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                }
                Event::End(TagEnd::CodeBlock) => {
                    in_code = false;
                    // Top border: ╭─ language ────────
                    let lang_display = if code_lang.is_empty() { "" } else { &code_lang };
                    if !lang_display.is_empty() {
                        lines.push(Line::from(vec![
                            Span::styled("  \u{256d}\u{2500} ", Style::default().fg(theme::md_code_border())),
                            Span::styled(
                                lang_display.to_string(),
                                Style::default().fg(theme::md_code_lang()).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!(" {}", "\u{2500}".repeat(30)),
                                Style::default().fg(theme::md_code_border()),
                            ),
                        ]));
                    } else {
                        lines.push(Line::from(Span::styled(
                            format!("  \u{256d}{}", "\u{2500}".repeat(36)),
                            Style::default().fg(theme::md_code_border()),
                        )));
                    }
                    let highlighted = self.highlight(&code_buf, &code_lang);
                    lines.extend(highlighted);
                    // Bottom border: ╰──────────
                    lines.push(Line::from(Span::styled(
                        format!("  \u{2570}{}", "\u{2500}".repeat(36)),
                        Style::default().fg(theme::md_code_border()),
                    )));
                    code_buf.clear();
                }

                // Paragraphs — tight spacing: no trailing blank line,
                // add one blank line before if previous content exists (avoids double blanks)
                Event::Start(Tag::Paragraph) => {
                    if !lines.is_empty() && lines.last().map_or(false, |l| !l.spans.is_empty()) {
                        lines.push(Line::default());
                    }
                }
                Event::End(TagEnd::Paragraph) => {
                    flush(&mut lines, &mut spans);
                }

                // Lists
                Event::Start(Tag::List(start)) => {
                    list_depth += 1;
                    ordered_idx = start;
                }
                Event::End(TagEnd::List(_)) => {
                    list_depth = list_depth.saturating_sub(1);
                    ordered_idx = None;
                }
                Event::Start(Tag::Item) => {
                    let indent = "  ".repeat(list_depth);
                    if let Some(idx) = &mut ordered_idx {
                        spans.push(Span::styled(
                            format!("{}{}. ", indent, idx),
                            Style::default().fg(theme::md_bullet()),
                        ));
                        *idx += 1;
                    } else {
                        spans.push(Span::styled(
                            format!("{}- ", indent),
                            Style::default().fg(theme::md_bullet()),
                        ));
                    }
                }
                Event::End(TagEnd::Item) => { flush(&mut lines, &mut spans); }

                // Block quotes
                Event::Start(Tag::BlockQuote(_)) => {
                    in_quote = true;
                    styles.push(Style::default().fg(theme::md_quote_text()));
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    in_quote = false;
                    styles.pop();
                    if !spans.is_empty() {
                        let mut row = vec![Span::styled("  \u{2502} ", Style::default().fg(theme::md_quote_bar()))];
                        row.extend(std::mem::take(&mut spans));
                        lines.push(Line::from(row));
                    }
                }

                // Tables
                Event::Start(Tag::Table(_)) => { table_header.clear(); table_rows.clear(); }
                Event::End(TagEnd::Table) => {
                    render_table(&mut lines, &table_header, &table_rows);
                    table_header.clear(); table_rows.clear();
                }
                Event::Start(Tag::TableHead) => { collecting_header = true; table_row.clear(); }
                Event::End(TagEnd::TableHead) => {
                    collecting_header = false;
                    table_header = table_row.clone();
                    table_row.clear();
                }
                Event::Start(Tag::TableRow) => { table_row.clear(); }
                Event::End(TagEnd::TableRow) => {
                    if !collecting_header { table_rows.push(table_row.clone()); }
                    table_row.clear();
                }
                Event::Start(Tag::TableCell) => {}
                Event::End(TagEnd::TableCell) => {
                    let cell: String = spans.iter().map(|s| s.content.to_string()).collect();
                    table_row.push(cell.trim().to_string());
                    spans.clear();
                }

                // Text
                Event::Text(text) => {
                    if in_code {
                        code_buf.push_str(&text);
                    } else if in_quote {
                        for (i, tl) in text.lines().enumerate() {
                            if i > 0 || !spans.is_empty() {
                                if !spans.is_empty() {
                                    let mut row = vec![Span::styled("  \u{2502} ", Style::default().fg(theme::md_quote_bar()))];
                                    row.extend(std::mem::take(&mut spans));
                                    lines.push(Line::from(row));
                                }
                            }
                            spans.push(Span::styled(tl.to_string(), cur_style(&styles)));
                        }
                    } else {
                        spans.push(Span::styled(text.to_string(), cur_style(&styles)));
                    }
                }
                Event::Code(code) => {
                    spans.push(Span::styled(
                        format!(" {} ", code),
                        Style::default().fg(theme::md_inline_code()).bg(theme::bg_inline_code()),
                    ));
                }
                Event::SoftBreak | Event::HardBreak => { flush(&mut lines, &mut spans); }
                Event::Rule => {
                    lines.push(Line::from(Span::styled("\u{2500}".repeat(40), Style::default().fg(theme::border()))));
                }
                _ => {}
            }
        }

        if !spans.is_empty() { lines.push(Line::from(spans)); }

        // Collapse consecutive blank lines and trim trailing blanks
        let mut deduped: Vec<Line<'static>> = Vec::with_capacity(lines.len());
        let mut prev_blank = false;
        for line in lines {
            let is_blank = line.spans.is_empty();
            if is_blank && prev_blank { continue; }
            prev_blank = is_blank;
            deduped.push(line);
        }
        // Remove trailing blank line
        if deduped.last().map_or(false, |l| l.spans.is_empty()) {
            deduped.pop();
        }
        deduped
    }

    /// Syntax-highlighted code with line numbers, left border, and background.
    fn highlight(&self, code: &str, lang: &str) -> Vec<Line<'static>> {
        let syntax = if lang.is_empty() {
            self.syntax_set.find_syntax_plain_text()
        } else {
            self.syntax_set.find_syntax_by_token(lang)
                .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
        };

        let mut hl = HighlightLines::new(syntax, &self.theme);
        let line_count = code.lines().count();
        let gutter_width = if line_count < 10 { 1 } else if line_count < 100 { 2 } else if line_count < 1000 { 3 } else { 4 };
        let gutter_bg = theme::md_code_gutter();

        code.lines().enumerate().map(|(i, line)| {
            let regions = hl.highlight_line(line, &self.syntax_set).unwrap_or_default();
            let mut s: Vec<Span<'static>> = Vec::new();

            // Left border
            s.push(Span::styled("  \u{2502}", Style::default().fg(theme::md_code_border())));

            // Line number gutter — dim, with subtle background
            s.push(Span::styled(
                format!(" {:>width$} ", i + 1, width = gutter_width),
                Style::default().fg(theme::md_code_linenum()).bg(gutter_bg),
            ));
            // Gutter separator
            s.push(Span::styled("\u{2502} ", Style::default().fg(theme::md_code_border()).bg(theme::bg_code())));

            // Highlighted code
            for (style, text) in regions {
                let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                s.push(Span::styled(text.to_string(), Style::default().fg(fg).bg(theme::bg_code())));
            }

            // Pad right side with background
            s.push(Span::styled("  ", Style::default().bg(theme::bg_code())));
            Line::from(s)
        }).collect()
    }
}

fn cur_style(stack: &[Style]) -> Style {
    *stack.last().unwrap_or(&Style::default().fg(theme::text_secondary()))
}

fn flush(lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

fn render_table(lines: &mut Vec<Line<'static>>, header: &[String], rows: &[Vec<String>]) {
    if header.is_empty() { return; }
    let cols = header.len();
    let border = Style::default().fg(theme::border());
    let hdr_style = Style::default().fg(theme::text_primary()).add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(theme::text_secondary());

    // Column widths: content + 2 padding each side
    let mut widths: Vec<usize> = header.iter().map(|h| display_width_str(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() { widths[i] = widths[i].max(display_width_str(cell)); }
        }
    }
    let widths: Vec<usize> = widths.iter().map(|w| w + 2).collect();

    lines.push(Line::default());

    // ┌───┬───┐
    let mut top = String::from("  \u{250c}");
    for (i, w) in widths.iter().enumerate() {
        top.push_str(&"\u{2500}".repeat(*w));
        top.push(if i < cols - 1 { '\u{252c}' } else { '\u{2510}' });
    }
    lines.push(Line::from(Span::styled(top, border)));

    // │ Header │
    let mut h: Vec<Span<'static>> = vec![Span::styled("  \u{2502}", border)];
    for (i, hdr) in header.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(4);
        let pad = w.saturating_sub(display_width_str(hdr) + 1);
        h.push(Span::styled(format!(" {}{}", hdr, " ".repeat(pad)), hdr_style));
        h.push(Span::styled("\u{2502}", border));
    }
    lines.push(Line::from(h));

    // ├───┼───┤
    let mut mid = String::from("  \u{251c}");
    for (i, w) in widths.iter().enumerate() {
        mid.push_str(&"\u{2500}".repeat(*w));
        mid.push(if i < cols - 1 { '\u{253c}' } else { '\u{2524}' });
    }
    lines.push(Line::from(Span::styled(mid, border)));

    // │ Data │
    for row in rows {
        let mut r: Vec<Span<'static>> = vec![Span::styled("  \u{2502}", border)];
        for i in 0..cols {
            let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
            let w = widths.get(i).copied().unwrap_or(4);
            let pad = w.saturating_sub(display_width_str(cell) + 1);
            r.push(Span::styled(format!(" {}{}", cell, " ".repeat(pad)), cell_style));
            r.push(Span::styled("\u{2502}", border));
        }
        lines.push(Line::from(r));
    }

    // └───┴───┘
    let mut bot = String::from("  \u{2514}");
    for (i, w) in widths.iter().enumerate() {
        bot.push_str(&"\u{2500}".repeat(*w));
        bot.push(if i < cols - 1 { '\u{2534}' } else { '\u{2518}' });
    }
    lines.push(Line::from(Span::styled(bot, border)));

    lines.push(Line::default());
}

/// Display width of a string (CJK = 2 columns).
fn display_width_str(s: &str) -> usize {
    s.chars().map(|c| {
        let cp = c as u32;
        if (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp)
            || (0xFF01..=0xFF60).contains(&cp) || (0x3000..=0x30FF).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp)
        { 2 } else { 1 }
    }).sum()
}

/// Wrap lines that exceed `max_width` columns.
/// Splits spans at character boundaries, preserving styles.
/// CJK characters count as 2 columns.
pub fn wrap_lines(lines: Vec<Line<'static>>, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 { return lines; }
    let mut result = Vec::with_capacity(lines.len());
    for line in lines {
        let total_w: usize = line.spans.iter().map(|s| span_width(s)).sum();
        if total_w <= max_width {
            result.push(line);
            continue;
        }
        // Need to wrap — split spans across multiple lines
        let mut cur_spans: Vec<Span<'static>> = Vec::new();
        let mut cur_w: usize = 0;
        for span in line.spans {
            let style = span.style;
            let text: &str = &span.content;
            let mut chars = text.chars().peekable();
            let mut buf = String::new();
            while let Some(c) = chars.next() {
                let cw = char_width(c);
                if cur_w + cw > max_width && cur_w > 0 {
                    // Emit current buffer as a span, start new line
                    if !buf.is_empty() {
                        cur_spans.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    result.push(Line::from(std::mem::take(&mut cur_spans)));
                    cur_w = 0;
                }
                buf.push(c);
                cur_w += cw;
            }
            if !buf.is_empty() {
                cur_spans.push(Span::styled(buf, style));
            }
        }
        if !cur_spans.is_empty() {
            result.push(Line::from(cur_spans));
        }
    }
    result
}

fn span_width(span: &Span) -> usize {
    span.content.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let cp = c as u32;
    if (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp)
        || (0x20000..=0x2A6DF).contains(&cp) || (0xF900..=0xFAFF).contains(&cp)
        || (0xFF01..=0xFF60).contains(&cp) || (0xFFE0..=0xFFE6).contains(&cp)
        || (0xAC00..=0xD7AF).contains(&cp) || (0x3000..=0x303F).contains(&cp)
        || (0x3040..=0x309F).contains(&cp) || (0x30A0..=0x30FF).contains(&cp)
    { 2 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text() {
        let lines = render_markdown("Hello world");
        assert!(!lines.is_empty());
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Hello world"));
    }

    #[test]
    fn test_bold_text() {
        let lines = render_markdown("Hello **bold** world");
        assert!(!lines.is_empty());
        let has_bold = lines.iter().any(|l| l.spans.iter().any(|s|
            s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(has_bold);
    }

    #[test]
    fn test_heading() {
        let lines = render_markdown("# Title");
        assert!(!lines.is_empty());
        let has_bold = lines.iter().any(|l| l.spans.iter().any(|s|
            s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(has_bold);
    }

    #[test]
    fn test_code_block() {
        let md = "```rust\nfn main() {}\n```";
        let lines = render_markdown(md);
        assert!(lines.len() >= 2);
        let has_lang = lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("rust")));
        assert!(has_lang);
    }

    #[test]
    fn test_empty_input() {
        let lines = render_markdown("");
        assert!(lines.is_empty() || lines.len() == 1);
    }
}
