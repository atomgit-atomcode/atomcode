use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use atomcode_core::conversation::Conversation;
use atomcode_core::conversation::message::Role;

use super::markdown::render_markdown;

pub fn render(frame: &mut Frame, area: Rect, conversation: &Conversation, scroll_offset: usize) {
    let mut all_lines: Vec<Line<'static>> = Vec::new();

    for msg in &conversation.messages {
        match msg.role {
            Role::User => {
                all_lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled(msg.content.clone(), Style::default().fg(Color::White)),
                ]));
                all_lines.push(Line::default());
            }
            Role::Assistant => {
                let md_lines = render_markdown(&msg.content);
                all_lines.extend(md_lines);
                all_lines.push(Line::default());
            }
            Role::System => {}
        }
    }

    // Render streaming buffer (partial assistant response)
    if let Some(ref buffer) = conversation.stream_buffer {
        let md_lines = render_markdown(buffer);
        all_lines.extend(md_lines);
    }

    let paragraph = Paragraph::new(all_lines)
        .scroll((scroll_offset as u16, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Calculate total line count for scroll calculations.
pub fn total_lines(conversation: &Conversation) -> usize {
    let mut count = 0;
    for msg in &conversation.messages {
        match msg.role {
            Role::User => count += 2,
            Role::Assistant => {
                count += render_markdown(&msg.content).len() + 1;
            }
            Role::System => {}
        }
    }
    if let Some(ref buffer) = conversation.stream_buffer {
        count += render_markdown(buffer).len();
    }
    count
}
