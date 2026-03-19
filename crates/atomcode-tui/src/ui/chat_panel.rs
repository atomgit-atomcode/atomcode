use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use atomcode_core::conversation::Conversation;
use atomcode_core::conversation::message::{MessageContent, Role};

use super::markdown::render_markdown;

/// Colors / styles
const USER_LABEL_COLOR: Color = Color::Rgb(100, 180, 255); // Light blue
const ASSISTANT_LABEL_COLOR: Color = Color::Rgb(180, 140, 255); // Purple
const SEPARATOR_COLOR: Color = Color::Rgb(50, 50, 50);
const USER_TEXT_COLOR: Color = Color::White;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    scroll_offset: usize,
    at_bottom: bool,
) {
    let mut all_lines: Vec<Line<'static>> = Vec::new();

    for msg in &conversation.messages {
        match &msg.content {
            MessageContent::Text(text) => {
                match msg.role {
                    Role::User => render_user_message(&mut all_lines, text),
                    Role::Assistant => render_assistant_message(&mut all_lines, text),
                    _ => {}
                }
            }
            MessageContent::AssistantWithToolCalls { text, .. } => {
                if let Some(t) = text {
                    render_assistant_message(&mut all_lines, t);
                }
            }
            MessageContent::ToolResult(_result) => {
                // Full rendering comes in Task 6
            }
        }
    }

    // Render streaming buffer (partial assistant response)
    if let Some(ref buffer) = conversation.stream_buffer {
        // Show assistant label for streaming content
        all_lines.push(Line::from(Span::styled(
            "  assistant",
            Style::default()
                .fg(ASSISTANT_LABEL_COLOR)
                .add_modifier(Modifier::BOLD),
        )));
        all_lines.push(Line::default());

        let md_lines = render_markdown(buffer);
        for line in md_lines {
            // Indent assistant content
            let mut spans = vec![Span::raw("  ".to_string())];
            spans.extend(line.spans);
            all_lines.push(Line::from(spans));
        }

        // Streaming cursor
        all_lines.push(Line::from(Span::styled(
            "  \u{2588}",
            Style::default().fg(ASSISTANT_LABEL_COLOR),
        )));
    }

    // Calculate auto-scroll
    let total = all_lines.len();
    let viewport_h = area.height as usize;
    let effective_scroll = if at_bottom {
        total.saturating_sub(viewport_h)
    } else {
        scroll_offset.min(total.saturating_sub(viewport_h))
    };

    let paragraph = Paragraph::new(all_lines)
        .scroll((effective_scroll as u16, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn render_user_message(lines: &mut Vec<Line<'static>>, content: &str) {
    // Separator
    lines.push(Line::default());

    // User label with icon
    lines.push(Line::from(vec![
        Span::styled(
            "  \u{f007} you",
            Style::default()
                .fg(USER_LABEL_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::default());

    // User content with indent
    for text_line in content.lines() {
        lines.push(Line::from(vec![
            Span::raw("  ".to_string()),
            Span::styled(text_line.to_string(), Style::default().fg(USER_TEXT_COLOR)),
        ]));
    }

    lines.push(Line::default());

    // Thin separator after user message
    lines.push(Line::from(Span::styled(
        "  \u{2500}".repeat(1) + &"\u{2500}".repeat(40),
        Style::default().fg(SEPARATOR_COLOR),
    )));
    lines.push(Line::default());
}

fn render_assistant_message(lines: &mut Vec<Line<'static>>, content: &str) {
    // Assistant label
    lines.push(Line::from(Span::styled(
        "  assistant",
        Style::default()
            .fg(ASSISTANT_LABEL_COLOR)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    // Markdown rendered content with indent
    let md_lines = render_markdown(content);
    for line in md_lines {
        let mut spans = vec![Span::raw("  ".to_string())];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }

    lines.push(Line::default());
}

/// Calculate total line count for scroll calculations.
pub fn total_lines(conversation: &Conversation) -> usize {
    let mut count = 0;
    for msg in &conversation.messages {
        match &msg.content {
            MessageContent::Text(text) => {
                match msg.role {
                    Role::User => {
                        // label(1) + blank(1) + content lines + blank(1) + separator(1) + blank(1) + top blank(1)
                        count += 6 + text.lines().count();
                    }
                    Role::Assistant => {
                        // label(1) + blank(1) + md lines + blank(1)
                        count += 3 + render_markdown(text).len();
                    }
                    _ => {}
                }
            }
            MessageContent::AssistantWithToolCalls { text, .. } => {
                if let Some(t) = text {
                    count += 3 + render_markdown(t).len();
                }
            }
            MessageContent::ToolResult(_) => {
                // Will be rendered in Task 6
            }
        }
    }
    if let Some(ref buffer) = conversation.stream_buffer {
        // label(1) + blank(1) + md lines + cursor(1)
        count += 3 + render_markdown(buffer).len();
    }
    count
}
