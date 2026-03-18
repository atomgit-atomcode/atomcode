use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

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
        let mut style_stack: Vec<Style> = vec![Style::default()];
        let mut in_code_block = false;
        let mut code_lang = String::new();
        let mut code_content = String::new();

        for event in parser {
            match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    let style = Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD);
                    style_stack.push(style);
                    let prefix = "#".repeat(level as usize);
                    current_spans.push(Span::styled(format!("{} ", prefix), style));
                }
                Event::End(TagEnd::Heading(_)) => {
                    style_stack.pop();
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
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
                        .fg(Color::Blue)
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
                    let highlighted = self.highlight_code(&code_content, &code_lang);
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
                Event::Start(Tag::List(_)) => {}
                Event::End(TagEnd::List(_)) => {}
                Event::Start(Tag::Item) => {
                    current_spans.push(Span::raw("  \u{2022} ".to_string()));
                }
                Event::End(TagEnd::Item) => {
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
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
                    let style = Style::default().bg(Color::DarkGray);
                    current_spans.push(Span::styled(format!(" {} ", code), style));
                }
                Event::SoftBreak | Event::HardBreak => {
                    if !current_spans.is_empty() {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                }
                _ => {}
            }
        }

        if !current_spans.is_empty() {
            lines.push(Line::from(current_spans));
        }

        lines
    }

    fn highlight_code(&self, code: &str, lang: &str) -> Vec<Line<'static>> {
        let syntax = if lang.is_empty() {
            self.syntax_set.find_syntax_plain_text()
        } else {
            self.syntax_set
                .find_syntax_by_token(lang)
                .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
        };

        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let bg = Color::Rgb(30, 30, 30);

        code.lines()
            .map(|line| {
                let regions = highlighter
                    .highlight_line(line, &self.syntax_set)
                    .unwrap_or_default();
                let spans: Vec<Span<'static>> = regions
                    .into_iter()
                    .map(|(style, text)| {
                        let fg = Color::Rgb(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        );
                        Span::styled(text.to_string(), Style::default().fg(fg).bg(bg))
                    })
                    .collect();
                Line::from(spans)
            })
            .collect()
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
        let has_bold_cyan = lines.iter().any(|line| {
            line.spans.iter().any(|s| {
                s.style.fg == Some(ratatui::style::Color::Cyan)
                    && s.style.add_modifier.contains(ratatui::style::Modifier::BOLD)
            })
        });
        assert!(has_bold_cyan);
    }

    #[test]
    fn test_code_block() {
        let md = "```rust\nfn main() {}\n```";
        let lines = render_markdown(md);
        assert!(lines.len() >= 1);
        let has_bg = lines
            .iter()
            .any(|line| line.spans.iter().any(|s| s.style.bg.is_some()));
        assert!(has_bg);
    }

    #[test]
    fn test_empty_input() {
        let lines = render_markdown("");
        assert!(lines.is_empty() || lines.len() == 1);
    }
}
