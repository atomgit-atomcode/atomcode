use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

const CODE_BG: Color = Color::Rgb(30, 30, 30);
const CODE_BORDER: Color = Color::Rgb(60, 60, 60);
const INLINE_CODE_BG: Color = Color::Rgb(45, 45, 45);
const HEADING_COLOR: Color = Color::Rgb(100, 200, 255);
const LINK_COLOR: Color = Color::Rgb(100, 150, 255);
const LIST_BULLET_COLOR: Color = Color::Rgb(120, 120, 120);

/// Render a markdown string into a Vec of ratatui Lines.
pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let renderer = MarkdownRenderer::new();
    renderer.render(input)
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
        let mut style_stack: Vec<Style> = vec![Style::default().fg(Color::Rgb(200, 200, 200))];
        let mut in_code_block = false;
        let mut code_lang = String::new();
        let mut code_content = String::new();
        let mut list_depth: usize = 0;
        let mut ordered_index: Option<u64> = None;

        for event in parser {
            match event {
                Event::Start(Tag::Heading { level: _, .. }) => {
                    let style = Style::default()
                        .fg(HEADING_COLOR)
                        .add_modifier(Modifier::BOLD);
                    style_stack.push(style);
                    // Don't show # prefix — just style the text
                }
                Event::End(TagEnd::Heading(_)) => {
                    style_stack.pop();
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                    lines.push(Line::default());
                }
                Event::Start(Tag::Strong) => {
                    let mut style = *style_stack.last().unwrap_or(&Style::default());
                    style = style.add_modifier(Modifier::BOLD);
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
                    let _ = dest_url;
                }
                Event::End(TagEnd::Link) => {
                    style_stack.pop();
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
                    lines.push(Line::default());
                }
                Event::Start(Tag::List(start)) => {
                    list_depth += 1;
                    ordered_index = start;
                }
                Event::End(TagEnd::List(_)) => {
                    list_depth = list_depth.saturating_sub(1);
                    if list_depth == 0 {
                        lines.push(Line::default());
                    }
                }
                Event::Start(Tag::Item) => {
                    let indent = "  ".repeat(list_depth);
                    if let Some(idx) = &mut ordered_index {
                        current_spans.push(Span::styled(
                            format!("{}{}. ", indent, idx),
                            Style::default().fg(LIST_BULLET_COLOR),
                        ));
                        *idx += 1;
                    } else {
                        current_spans.push(Span::styled(
                            format!("{}\u{2022} ", indent),
                            Style::default().fg(LIST_BULLET_COLOR),
                        ));
                    }
                }
                Event::End(TagEnd::Item) => {
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    let style = Style::default().fg(Color::Rgb(140, 140, 140));
                    style_stack.push(style);
                    current_spans.push(Span::styled(
                        "\u{2502} ".to_string(),
                        Style::default().fg(Color::Rgb(80, 80, 80)),
                    ));
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    style_stack.pop();
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                    lines.push(Line::default());
                }
                Event::Text(text) => {
                    if in_code_block {
                        code_content.push_str(&text);
                    } else {
                        let style = *style_stack.last().unwrap_or(&Style::default());
                        current_spans.push(Span::styled(text.to_string(), style));
                    }
                }
                Event::Code(code) => {
                    let style = Style::default()
                        .fg(Color::Rgb(220, 180, 120))
                        .bg(INLINE_CODE_BG);
                    current_spans.push(Span::styled(format!(" {} ", code), style));
                }
                Event::SoftBreak | Event::HardBreak => {
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                }
                Event::Rule => {
                    lines.push(Line::from(Span::styled(
                        "\u{2500}".repeat(50),
                        Style::default().fg(Color::Rgb(60, 60, 60)),
                    )));
                    lines.push(Line::default());
                }
                _ => {}
            }
        }

        if !current_spans.is_empty() {
            lines.push(Line::from(current_spans));
        }

        lines
    }

    /// Render a code block with language label, border, and syntax highlighting.
    fn render_code_block(&self, code: &str, lang: &str) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let border_style = Style::default().fg(CODE_BORDER);

        // Top border with language label
        if !lang.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("\u{256d}\u{2500} ", border_style),
                Span::styled(
                    lang.to_string(),
                    Style::default()
                        .fg(Color::Rgb(150, 150, 150))
                        .add_modifier(Modifier::ITALIC),
                ),
                Span::styled(
                    format!(" {}", "\u{2500}".repeat(40)),
                    border_style,
                ),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                format!("\u{256d}{}", "\u{2500}".repeat(44)),
                border_style,
            )));
        }

        // Code lines with syntax highlighting
        let syntax = if lang.is_empty() {
            self.syntax_set.find_syntax_plain_text()
        } else {
            self.syntax_set
                .find_syntax_by_token(lang)
                .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
        };

        let mut highlighter = HighlightLines::new(syntax, &self.theme);

        for code_line in code.lines() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled("\u{2502} ", border_style));

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
            format!("\u{2570}{}", "\u{2500}".repeat(44)),
            border_style,
        )));
        lines.push(Line::default());

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
                .any(|s| s.style.add_modifier.contains(ratatui::style::Modifier::BOLD))
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
                .any(|s| s.style.add_modifier.contains(ratatui::style::Modifier::BOLD))
        });
        assert!(has_bold);
    }

    #[test]
    fn test_code_block() {
        let md = "```rust\nfn main() {}\n```";
        let lines = render_markdown(md);
        assert!(lines.len() >= 3); // top border + code + bottom border
        // Should have language label
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
