use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::ui::theme;

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

        if is_selected {
            // Selected: accent background, white text
            let marker_color = if is_current {
                theme::text_on_accent()
            } else {
                theme::text_on_accent()
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", marker),
                    Style::default().fg(marker_color).bg(theme::accent()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<14}", name),
                    Style::default().fg(theme::text_on_accent()).bg(theme::accent()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {}", model),
                    Style::default().fg(theme::text_on_accent()).bg(theme::accent()),
                ),
            ]));
        } else {
            // Not selected: normal colors with explicit background
            let marker_style = if is_current {
                Style::default().fg(theme::success()).bg(theme::bg_elevated()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::text_muted()).bg(theme::bg_elevated())
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", marker), marker_style),
                Span::styled(format!("{:<14}", name), Style::default().fg(theme::text_primary()).bg(theme::bg_elevated())),
                Span::styled(format!(" {}", model), Style::default().fg(theme::text_muted()).bg(theme::bg_elevated())),
            ]));
        }
    }

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  Enter", Style::default().fg(theme::text_secondary())),
        Span::styled(" select  ", Style::default().fg(theme::text_muted())),
        Span::styled("Esc", Style::default().fg(theme::text_secondary())),
        Span::styled(" cancel", Style::default().fg(theme::text_muted())),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::border()))
        .title(Span::styled(
            " Switch Model ",
            Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD),
        ));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .style(Style::default().bg(theme::bg_elevated()));

    frame.render_widget(paragraph, popup);
}
