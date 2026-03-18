use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::InputState;

pub fn render(frame: &mut Frame, area: Rect, input: &InputState) {
    let lines: Vec<Line> = input
        .lines
        .iter()
        .map(|l| Line::from(Span::raw(l.clone())))
        .collect();

    let input_widget = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " > ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .style(Style::default().fg(Color::White));

    frame.render_widget(input_widget, area);

    // Set cursor position
    let cursor_x = area.x + input.cursor_col as u16 + 1;
    let cursor_y = area.y + input.cursor_row as u16 + 1;
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Calculate how tall the input box should be.
pub fn height(input: &InputState, terminal_height: u16) -> u16 {
    let content_lines = input.lines.len() as u16;
    let max_height = terminal_height / 2;
    let min_height = 3;
    (content_lines + 2).clamp(min_height, max_height)
}
