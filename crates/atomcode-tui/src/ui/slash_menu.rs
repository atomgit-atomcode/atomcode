use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::command::{SlashMenu, COMMANDS};

/// Render the slash command autocomplete menu as a floating popup above the input box.
pub fn render(frame: &mut Frame, input_area: Rect, menu: &SlashMenu) {
    if !menu.visible || menu.filtered.is_empty() {
        return;
    }

    let item_count = menu.filtered.len();
    let menu_height = (item_count as u16 + 2).min(14); // +2 for border, max 14
    let menu_width = 50u16.min(input_area.width.saturating_sub(2));

    // Position: above the input box, aligned to left
    let menu_y = input_area.y.saturating_sub(menu_height);
    let menu_x = input_area.x + 1;

    let area = Rect::new(menu_x, menu_y, menu_width, menu_height);

    // Build menu items
    let lines: Vec<Line> = menu
        .filtered
        .iter()
        .enumerate()
        .map(|(i, &cmd_idx)| {
            let cmd = &COMMANDS[cmd_idx];
            let is_selected = i == menu.selected;

            if is_selected {
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", cmd.name),
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {}", cmd.description),
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", cmd.name),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!(" {}", cmd.description),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            }
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(Color::Rgb(80, 80, 80)))
        .title(Span::styled(
            " Commands ",
            Style::default().fg(Color::Gray),
        ));

    // Clear the area first (so menu floats above content)
    frame.render_widget(Clear, area);

    let paragraph = Paragraph::new(lines)
        .block(block)
        .style(Style::default().bg(Color::Rgb(30, 30, 30)));

    frame.render_widget(paragraph, area);
}
