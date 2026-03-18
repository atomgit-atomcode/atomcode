use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::InputState;

pub fn render(frame: &mut Frame, area: Rect, input: &InputState, is_streaming: bool) {
    let is_empty = input.is_empty();

    // Build content lines
    let lines: Vec<Line> = if is_empty && !is_streaming {
        // Placeholder hint when empty
        vec![Line::from(Span::styled(
            "Ask anything... (ctrl+j to send, /config for settings)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        input
            .lines
            .iter()
            .map(|l| Line::from(Span::raw(l.clone())))
            .collect()
    };

    // Border color changes based on state
    let border_color = if is_streaming {
        Color::Yellow
    } else {
        Color::Rgb(100, 100, 100)
    };

    let prompt = if is_streaming {
        Span::styled(" ... ", Style::default().fg(Color::Yellow))
    } else {
        Span::styled(
            " > ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(border_color))
        .title(prompt);

    let input_widget = Paragraph::new(lines)
        .block(block)
        .style(Style::default().fg(Color::White));

    frame.render_widget(input_widget, area);

    // Set cursor position (inside the border: +1 for left border)
    if !is_streaming {
        let cursor_x = area.x + input.cursor_col as u16 + 1;
        let cursor_y = area.y + input.cursor_row as u16 + 1;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Calculate how tall the input box should be.
/// Min 3, max 50% of terminal, based on content lines.
pub fn height(input: &InputState, terminal_height: u16) -> u16 {
    let content_lines = input.lines.len() as u16;
    let max_height = terminal_height / 2;
    let min_height = 3;
    // +2 for top and bottom borders
    (content_lines + 2).clamp(min_height, max_height)
}
