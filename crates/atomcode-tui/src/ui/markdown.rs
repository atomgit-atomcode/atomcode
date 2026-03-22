use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, HeadingLevel};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

// Claude Code-inspired palette: clean, bright, minimal
const TEXT: Color = Color::Rgb(220, 220, 225);
const BOLD_TEXT: Color = Color::White;
const H1: Color = Color::Rgb(130, 190, 255);
const H2: Color = Color::Rgb(180, 160, 255);
const H3: Color = Color::Rgb(150, 210, 180);
const LINK: Color = Color::Rgb(110, 160, 255);
const DIM: Color = Color::Rgb(110, 110, 120);
const INLINE_CODE_FG: Color = Color::Rgb(240, 200, 140);
const INLINE_CODE_BG: Color = Color::Rgb(40, 38, 48);
const CODE_BG: Color = Color::Rgb(20, 20, 28);
const CODE_LABEL: Color = Color::Rgb(90, 90, 100);
const BULLET: Color = Color::Rgb(100, 120, 145);
const QUOTE_BAR: Color = Color::Rgb(60, 60, 75);
const QUOTE_TEXT: Color = Color::Rgb(170, 170, 180);
const RULE: Color = Color::Rgb(45, 45, 55);

pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    static RENDERER: std::sync::OnceLock<MarkdownRenderer> = std::sync::OnceLock::new();
    let renderer = RENDERER.get_or_init(MarkdownRenderer::new);
    let cleaned = strip_emoji(input);
    renderer.render(&cleaned)
}

fn strip_emoji(s: &str) -> String {
    s.chars().filter(|c| {
        let cp = *c as u32;
        !((0x1F600..=0x1F64F).contains(&cp) || (0x1F300..=0x1F5FF).contains(&cp) ||
          (0x1F680..=0x1F6FF).contains(&cp) || (0x1F1E0..=0x1F1FF).contains(&cp) ||
          (0x2600..=0x26FF).contains(&cp) || (0x2700..=0x27BF).contains(&cp) ||
          (0xFE00..=0xFE0F).contains(&cp) || (0x1F900..=0x1F9FF).contains(&cp) ||
          (0x1FA00..=0x1FA6F).contains(&cp) || (0x1FA70..=0x1FAFF).contains(&cp) ||
          cp == 0x200D || (0xE0020..=0xE007F).contains(&cp))
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
        let mut styles: Vec<Style> = vec![Style::default().fg(TEXT)];
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
                        HeadingLevel::H1 => H1,
                        HeadingLevel::H2 => H2,
                        _ => H3,
                    };
                    styles.push(Style::default().fg(color).add_modifier(Modifier::BOLD));
                }
                Event::End(TagEnd::Heading(_level)) => {
                    styles.pop();
                    flush(&mut lines, &mut spans);
                }

                // Inline styles
                Event::Start(Tag::Strong) => {
                    let s = cur_style(&styles).fg(BOLD_TEXT).add_modifier(Modifier::BOLD);
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
                    styles.push(Style::default().fg(LINK).add_modifier(Modifier::UNDERLINED));
                    link_url = Some(dest_url.to_string());
                }
                Event::End(TagEnd::Link) => {
                    styles.pop();
                    if let Some(url) = link_url.take() {
                        let last = spans.last().map(|s| s.content.to_string()).unwrap_or_default();
                        if !last.is_empty() && last != url && !url.is_empty() {
                            spans.push(Span::styled(format!(" ({})", url), Style::default().fg(DIM)));
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
                    // Top border with language label (Claude Code style)
                    let lang_display = if code_lang.is_empty() { "text" } else { &code_lang };
                    lines.push(Line::from(vec![
                        Span::styled("  \u{256d}\u{2500} ", Style::default().fg(Color::Rgb(50, 55, 65))),
                        Span::styled(
                            format!("{} ", lang_display),
                            Style::default().fg(Color::Rgb(120, 130, 150)).bg(Color::Rgb(30, 32, 40)),
                        ),
                        Span::styled(
                            "\u{2500}".repeat(30),
                            Style::default().fg(Color::Rgb(40, 43, 52)),
                        ),
                    ]));
                    // Code lines with syntax highlight
                    let highlighted = self.highlight(&code_buf, &code_lang);
                    lines.extend(highlighted);
                    // Bottom border
                    lines.push(Line::from(Span::styled(
                        format!("  \u{2570}{}", "\u{2500}".repeat(34)),
                        Style::default().fg(Color::Rgb(40, 43, 52)),
                    )));
                    code_buf.clear();
                }

                // Paragraphs — no extra blank line after, just flush
                Event::Start(Tag::Paragraph) => {}
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
                    let indent = "   ".repeat(list_depth);
                    if let Some(idx) = &mut ordered_idx {
                        spans.push(Span::styled(
                            format!("{}{:>2}. ", indent, idx),
                            Style::default().fg(BULLET),
                        ));
                        *idx += 1;
                    } else {
                        spans.push(Span::styled(
                            format!("{}  - ", indent),
                            Style::default().fg(BULLET),
                        ));
                    }
                }
                Event::End(TagEnd::Item) => { flush(&mut lines, &mut spans); }

                // Block quotes
                Event::Start(Tag::BlockQuote(_)) => {
                    in_quote = true;
                    styles.push(Style::default().fg(QUOTE_TEXT));
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    in_quote = false;
                    styles.pop();
                    if !spans.is_empty() {
                        let mut row = vec![Span::styled("  \u{2502} ", Style::default().fg(QUOTE_BAR))];
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
                                    let mut row = vec![Span::styled("  \u{2502} ", Style::default().fg(QUOTE_BAR))];
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
                        Style::default().fg(INLINE_CODE_FG).bg(INLINE_CODE_BG),
                    ));
                }
                Event::SoftBreak | Event::HardBreak => { flush(&mut lines, &mut spans); }
                Event::Rule => {
                    lines.push(Line::from(Span::styled("\u{2500}".repeat(40), Style::default().fg(RULE))));
                }
                _ => {}
            }
        }

        if !spans.is_empty() { lines.push(Line::from(spans)); }
        lines
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
        let border_style = Style::default().fg(Color::Rgb(50, 55, 65));
        let line_num_style = Style::default().fg(Color::Rgb(75, 80, 95)).bg(CODE_BG);
        let gutter_sep = Style::default().fg(Color::Rgb(50, 53, 63)).bg(CODE_BG);

        code.lines().enumerate().map(|(i, line)| {
            let regions = hl.highlight_line(line, &self.syntax_set).unwrap_or_default();
            let mut s: Vec<Span<'static>> = Vec::new();

            // Left border
            s.push(Span::styled("  \u{2502}", border_style));

            // Line number gutter
            s.push(Span::styled(
                format!(" {:>width$}", i + 1, width = gutter_width),
                line_num_style,
            ));
            s.push(Span::styled(" \u{2502} ", gutter_sep));

            // Highlighted code
            for (style, text) in regions {
                let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                s.push(Span::styled(text.to_string(), Style::default().fg(fg).bg(CODE_BG)));
            }

            // Pad right side with background
            s.push(Span::styled("  ", Style::default().bg(CODE_BG)));
            Line::from(s)
        }).collect()
    }
}

fn cur_style(stack: &[Style]) -> Style {
    *stack.last().unwrap_or(&Style::default().fg(TEXT))
}

fn flush(lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

fn render_table(lines: &mut Vec<Line<'static>>, header: &[String], rows: &[Vec<String>]) {
    if header.is_empty() { return; }
    let cols = header.len();

    // Calculate column widths: content + 4 chars padding (generous spacing)
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() { widths[i] = widths[i].max(cell.chars().count()); }
        }
    }
    let widths: Vec<usize> = widths.iter().map(|w| w + 4).collect();

    lines.push(Line::default());

    // Header row
    let mut h: Vec<Span<'static>> = Vec::new();
    for (i, hdr) in header.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(10);
        h.push(Span::styled(
            format!("  {:<width$}", hdr, width = w),
            Style::default().fg(BOLD_TEXT).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(h));

    // Separator — one continuous line
    let total_width: usize = widths.iter().sum::<usize>() + 2 * cols;
    lines.push(Line::from(Span::styled(
        format!("  {}", "\u{2500}".repeat(total_width)),
        Style::default().fg(RULE),
    )));

    // Data rows
    for row in rows {
        let mut r: Vec<Span<'static>> = Vec::new();
        for i in 0..cols {
            let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
            let w = widths.get(i).copied().unwrap_or(10);
            r.push(Span::styled(
                format!("  {:<width$}", cell, width = w),
                Style::default().fg(TEXT),
            ));
        }
        lines.push(Line::from(r));
    }

    lines.push(Line::default());
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
