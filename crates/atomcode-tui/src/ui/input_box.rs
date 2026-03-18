use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::app::InputState;

/// Left/right inner padding for the input box content.
const H_PADDING: u16 = 15;

pub fn render(frame: &mut Frame, area: Rect, input: &InputState, is_streaming: bool) {
    let is_empty = input.is_empty();

    // Build content lines
    let lines: Vec<Line> = if is_empty && !is_streaming {
        vec![Line::from(Span::styled(
            "Ask anything... (Enter to send, / for commands)",
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
        .title(prompt)
        .padding(Padding::horizontal(H_PADDING));

    let input_widget = Paragraph::new(lines)
        .block(block)
        .style(Style::default().fg(Color::White));

    frame.render_widget(input_widget, area);

    // Set cursor position:
    //   x = area.x + 1 (left border) + H_PADDING + cursor_col
    //   y = area.y + 1 (top border) + cursor_row
    if !is_streaming {
        let cursor_x = area.x + 1 + H_PADDING + input.cursor_col as u16;
        let cursor_y = area.y + 1 + input.cursor_row as u16;
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
