use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, HeadingLevel};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

// Clean, readable palette — bright enough to read, muted enough to not strain
const TEXT: Color = Color::Rgb(210, 210, 215);
const BOLD_TEXT: Color = Color::Rgb(245, 245, 250);
const H1_COLOR: Color = Color::Rgb(120, 180, 255);
const H2_COLOR: Color = Color::Rgb(170, 150, 255);
const H3_COLOR: Color = Color::Rgb(140, 200, 170);
const LINK_COLOR: Color = Color::Rgb(100, 150, 255);
const INLINE_CODE_FG: Color = Color::Rgb(220, 185, 140);
const INLINE_CODE_BG: Color = Color::Rgb(38, 38, 46);
const CODE_BG: Color = Color::Rgb(22, 22, 30);
const CODE_BORDER: Color = Color::Rgb(48, 48, 56);
const BULLET_COLOR: Color = Color::Rgb(120, 130, 150);
const QUOTE_BAR: Color = Color::Rgb(70, 70, 85);
const QUOTE_TEXT: Color = Color::Rgb(160, 160, 175);
const DIM: Color = Color::Rgb(100, 100, 110);
const RULE_COLOR: Color = Color::Rgb(50, 50, 60);

pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    static RENDERER: std::sync::OnceLock<MarkdownRenderer> = std::sync::OnceLock::new();
    let renderer = RENDERER.get_or_init(MarkdownRenderer::new);
    // Strip emoji characters before rendering
    let cleaned = strip_emoji(input);
    renderer.render(&cleaned)
}

/// Remove emoji and other decorative unicode from text.
fn strip_emoji(s: &str) -> String {
    s.chars().filter(|c| {
        let cp = *c as u32;
        // Keep basic ASCII, CJK, and standard unicode
        // Filter out emoji ranges
        !(
            (0x1F600..=0x1F64F).contains(&cp) || // Emoticons
            (0x1F300..=0x1F5FF).contains(&cp) || // Misc Symbols
            (0x1F680..=0x1F6FF).contains(&cp) || // Transport
            (0x1F1E0..=0x1F1FF).contains(&cp) || // Flags
            (0x2600..=0x26FF).contains(&cp) ||   // Misc symbols
            (0x2700..=0x27BF).contains(&cp) ||   // Dingbats
            (0xFE00..=0xFE0F).contains(&cp) ||   // Variation selectors
            (0x1F900..=0x1F9FF).contains(&cp) || // Supplemental
            (0x1FA00..=0x1FA6F).contains(&cp) || // Chess symbols etc
            (0x1FA70..=0x1FAFF).contains(&cp) || // Symbols extended
            (0x200D == cp) ||                     // Zero-width joiner
            (0xE0020..=0xE007F).contains(&cp)    // Tags
        )
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
        let mut current_spans: Vec<Span<'static>> = Vec::new();
        let mut style_stack: Vec<Style> = vec![Style::default().fg(TEXT)];
        let mut in_code_block = false;
        let mut code_lang = String::new();
        let mut code_content = String::new();
        let mut link_url: Option<String> = None;
        let mut _in_table = false;
        let mut table_row: Vec<String> = Vec::new();
        let mut list_depth: usize = 0;
        let mut ordered_index: Option<u64> = None;
        let mut in_blockquote = false;

        for event in parser {
            match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    // Blank line before heading for breathing room
                    if !lines.is_empty() {
                        lines.push(Line::default());
                    }
                    let color = match level {
                        HeadingLevel::H1 => H1_COLOR,
                        HeadingLevel::H2 => H2_COLOR,
                        _ => H3_COLOR,
                    };
                    let style = Style::default()
                        .fg(color)
                        .add_modifier(Modifier::BOLD);
                    style_stack.push(style);
                }
                Event::End(TagEnd::Heading(level)) => {
                    style_stack.pop();
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                    // Underline for H1
                    if level == HeadingLevel::H1 {
                        lines.push(Line::from(Span::styled(
                            "\u{2500}".repeat(40),
                            Style::default().fg(RULE_COLOR),
                        )));
                    }
                }
                Event::Start(Tag::Strong) => {
                    let mut style = *style_stack.last().unwrap_or(&Style::default());
                    style = style.fg(BOLD_TEXT).add_modifier(Modifier::BOLD);
                    style_stack.push(style);
                }
                Event::End(TagEnd::Strong) => {
                    style_stack.pop();
                }
                Event::Start(Tag::Emphasis) => {
                    let mut style = *style_stack.last().unwrap_or(&Style::default());
                    style = style.add_modifier(Modifier::ITALIC);
                    style_stack.push(style);
                }
                Event::End(TagEnd::Emphasis) => {
                    style_stack.pop();
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let style = Style::default()
                        .fg(LINK_COLOR)
                        .add_modifier(Modifier::UNDERLINED);
                    style_stack.push(style);
                    link_url = Some(dest_url.to_string());
                }
                Event::End(TagEnd::Link) => {
                    style_stack.pop();
                    // Show URL after link text if different from text
                    if let Some(url) = link_url.take() {
                        let last_text: String = current_spans.last()
                            .map(|s| s.content.to_string())
                            .unwrap_or_default();
                        if !last_text.is_empty() && last_text != url && !url.is_empty() {
                            current_spans.push(Span::styled(
                                format!(" ({})", url),
                                Style::default().fg(DIM),
                            ));
                        }
                    }
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    in_code_block = true;
                    code_content.clear();
                    code_lang = match kind {
                        CodeBlockKind::Fenced(lang) => lang.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                }
                Event::End(TagEnd::CodeBlock) => {
                    in_code_block = false;
                    let highlighted = self.render_code_block(&code_content, &code_lang);
                    lines.extend(highlighted);
                    code_content.clear();
                }
                Event::Start(Tag::Paragraph) => {}
                Event::End(TagEnd::Paragraph) => {
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                    // Single blank line between paragraphs
                    lines.push(Line::default());
                }
                Event::Start(Tag::List(start)) => {
                    list_depth += 1;
                    ordered_index = start;
                }
                Event::End(TagEnd::List(_)) => {
                    list_depth = list_depth.saturating_sub(1);
                    ordered_index = None;
                }
                Event::Start(Tag::Item) => {
                    let indent = "  ".repeat(list_depth);
                    if let Some(idx) = &mut ordered_index {
                        current_spans.push(Span::styled(
                            format!("{}{:>2}. ", indent, idx),
                            Style::default().fg(BULLET_COLOR),
                        ));
                        *idx += 1;
                    } else {
                        current_spans.push(Span::styled(
                            format!("{}  - ", indent),
                            Style::default().fg(BULLET_COLOR),
                        ));
                    }
                }
                Event::End(TagEnd::Item) => {
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    in_blockquote = true;
                    let style = Style::default().fg(QUOTE_TEXT);
                    style_stack.push(style);
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    in_blockquote = false;
                    style_stack.pop();
                    if !current_spans.is_empty() {
                        // Prepend quote bar
                        let mut with_bar = vec![Span::styled(
                            "  \u{2502} ".to_string(),
                            Style::default().fg(QUOTE_BAR),
                        )];
                        with_bar.extend(std::mem::take(&mut current_spans));
                        lines.push(Line::from(with_bar));
                    }
                }
                Event::Text(text) => {
                    if in_code_block {
                        code_content.push_str(&text);
                    } else {
                        let style = *style_stack.last().unwrap_or(&Style::default());
                        if in_blockquote {
                            // Add quote bar prefix for each line in blockquote
                            for (i, tline) in text.lines().enumerate() {
                                if i > 0 || !current_spans.is_empty() {
                                    if !current_spans.is_empty() {
                                        let mut with_bar = vec![Span::styled(
                                            "  \u{2502} ".to_string(),
                                            Style::default().fg(QUOTE_BAR),
                                        )];
                                        with_bar.extend(std::mem::take(&mut current_spans));
                                        lines.push(Line::from(with_bar));
                                    }
                                }
                                current_spans.push(Span::styled(tline.to_string(), style));
                            }
                        } else {
                            current_spans.push(Span::styled(text.to_string(), style));
                        }
                    }
                }
                Event::Code(code) => {
                    current_spans.push(Span::styled(
                        format!(" {} ", code),
                        Style::default().fg(INLINE_CODE_FG).bg(INLINE_CODE_BG),
                    ));
                }
                Event::SoftBreak | Event::HardBreak => {
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                }
                Event::Rule => {
                    lines.push(Line::from(Span::styled(
                        "  \u{2500}".repeat(20),
                        Style::default().fg(RULE_COLOR),
                    )));
                    lines.push(Line::default());
                }
                // Table support
                Event::Start(Tag::Table(_)) => {
                    _in_table = true;
                }
                Event::End(TagEnd::Table) => {
                    _in_table = false;
                    lines.push(Line::default());
                }
                Event::Start(Tag::TableHead) => {
                    table_row.clear();
                }
                Event::End(TagEnd::TableHead) => {
                    // Render header row
                    let header = table_row.join("  \u{2502}  ");
                    lines.push(Line::from(Span::styled(
                        header,
                        Style::default().fg(BOLD_TEXT).add_modifier(Modifier::BOLD),
                    )));
                    // Separator
                    lines.push(Line::from(Span::styled(
                        "\u{2500}".repeat(50),
                        Style::default().fg(RULE_COLOR),
                    )));
                    table_row.clear();
                }
                Event::Start(Tag::TableRow) => {
                    table_row.clear();
                }
                Event::End(TagEnd::TableRow) => {
                    let row = table_row.join("  \u{2502}  ");
                    lines.push(Line::from(Span::styled(
                        row,
                        Style::default().fg(TEXT),
                    )));
                    table_row.clear();
                }
                Event::Start(Tag::TableCell) => {}
                Event::End(TagEnd::TableCell) => {
                    // Collect cell text from current_spans
                    let cell_text: String = current_spans.iter()
                        .map(|s| s.content.to_string())
                        .collect();
                    table_row.push(cell_text);
                    current_spans.clear();
                }
                _ => {}
            }
        }

        if !current_spans.is_empty() {
            lines.push(Line::from(current_spans));
        }

        lines
    }

    fn render_code_block(&self, code: &str, lang: &str) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let border = Style::default().fg(CODE_BORDER);

        // Top border
        let label = if !lang.is_empty() {
            format!(" {} ", lang)
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled("\u{256d}\u{2500}", border),
            Span::styled(
                label,
                Style::default().fg(Color::Rgb(120, 120, 130)).add_modifier(Modifier::ITALIC),
            ),
            Span::styled(
                "\u{2500}".repeat(38),
                border,
            ),
        ]));

        // Code
        let syntax = if lang.is_empty() {
            self.syntax_set.find_syntax_plain_text()
        } else {
            self.syntax_set
                .find_syntax_by_token(lang)
                .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
        };

        let mut highlighter = HighlightLines::new(syntax, &self.theme);

        for code_line in code.lines() {
            let mut spans: Vec<Span<'static>> = vec![
                Span::styled("\u{2502} ", border),
            ];

            let regions = highlighter
                .highlight_line(code_line, &self.syntax_set)
                .unwrap_or_default();

            for (style, text) in regions {
                let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                spans.push(Span::styled(
                    text.to_string(),
                    Style::default().fg(fg).bg(CODE_BG),
                ));
            }

            lines.push(Line::from(spans));
        }

        // Bottom border
        lines.push(Line::from(Span::styled(
            format!("\u{2570}{}", "\u{2500}".repeat(40)),
            border,
        )));

        lines
    }
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
        let has_bold = lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        });
        assert!(has_bold);
    }

    #[test]
    fn test_heading() {
        let lines = render_markdown("# Title");
        assert!(!lines.is_empty());
        let has_bold = lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        });
        assert!(has_bold);
    }

    #[test]
    fn test_code_block() {
        let md = "```rust\nfn main() {}\n```";
        let lines = render_markdown(md);
        assert!(lines.len() >= 3);
        let has_lang = lines.iter().any(|line| {
            line.spans.iter().any(|s| s.content.contains("rust"))
        });
        assert!(has_lang);
    }

    #[test]
    fn test_empty_input() {
        let lines = render_markdown("");
        assert!(lines.is_empty() || lines.len() == 1);
    }
}
