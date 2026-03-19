use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;

const BG: Color = Color::Rgb(25, 25, 32);
const SELECTED_BG: Color = Color::Rgb(45, 42, 65);
const ACCENT: Color = Color::Rgb(130, 100, 255);
const DIM: Color = Color::Rgb(90, 90, 100);

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let item_count = app.model_list.len();
    let menu_height = (item_count as u16 + 4).min(area.height.saturating_sub(4));
    let menu_width = 60u16.min(area.width.saturating_sub(8));

    // Center the popup
    let x = area.x + (area.width.saturating_sub(menu_width)) / 2;
    let y = area.y + (area.height.saturating_sub(menu_height)) / 3;
    let popup = Rect::new(x, y, menu_width, menu_height);

    frame.render_widget(Clear, popup);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::default());

    for (i, (name, model)) in app.model_list.iter().enumerate() {
        let is_selected = i == app.model_selected;
        let is_current = *name == app.config.default_provider;

        let marker = if is_current { "*" } else { " " };
        let line_style = if is_selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default()
        };

        let marker_style = if is_current {
            Style::default().fg(Color::Rgb(80, 200, 120)).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };

        let name_style = if is_selected {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD).bg(SELECTED_BG)
        } else {
            Style::default().fg(Color::Rgb(180, 180, 190))
        };

        let model_style = if is_selected {
            Style::default().fg(Color::Rgb(160, 160, 170)).bg(SELECTED_BG)
        } else {
            Style::default().fg(DIM)
        };

        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", marker), marker_style.patch(line_style)),
            Span::styled(format!("{:<14}", name), name_style),
            Span::styled(format!(" {}", model), model_style),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  Enter", Style::default().fg(Color::Rgb(160, 160, 170))),
        Span::styled(" select  ", Style::default().fg(DIM)),
        Span::styled("Esc", Style::default().fg(Color::Rgb(160, 160, 170))),
        Span::styled(" cancel", Style::default().fg(DIM)),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(Color::Rgb(60, 55, 80)))
        .title(Span::styled(
            " Switch Model ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .style(Style::default().bg(BG));

    frame.render_widget(paragraph, popup);
}
