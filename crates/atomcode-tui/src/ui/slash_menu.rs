use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme;
use crate::command::{CommandKind, SlashMenu};

/// Render the slash command autocomplete menu as a floating popup above the input box.
pub fn render(frame: &mut Frame, input_area: Rect, menu: &SlashMenu) {
    if !menu.visible || menu.filtered.is_empty() {
        return;
    }
    let frame_area = frame.area();
    let item_count = menu.filtered.len();
    let max_visible = 12usize;
    let visible_count = item_count.min(max_visible);
    let menu_height = (visible_count as u16 + 2).min(14); // +2 for border, max 14
    let menu_width = 64u16.min(input_area.width.saturating_sub(2));

    // Position: above the input box, clamped to frame bounds
    let menu_y = input_area.y.saturating_sub(menu_height);
    let menu_x = input_area.x + 1;

    // Clamp height so the menu never exceeds the frame
    let clamped_height = menu_height.min(frame_area.height.saturating_sub(menu_y));
    if clamped_height < 3 || menu_width < 10 {
        return; // Too small to render anything useful
    }

    let area = Rect::new(menu_x, menu_y, menu_width, clamped_height);

    // Build menu items - only visible range
    let scroll_offset = menu.scroll_offset;
    let lines: Vec<Line> = menu
        .filtered
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_count)
        .map(|(i, entry)| {
            let is_selected = i == menu.selected;
            // Built-ins: accent color; skills: success color
            let name_color = match entry.kind {
                CommandKind::BuiltIn => theme::accent(),
                CommandKind::Skill => theme::success(),
            };

            let hint_str = entry
                .argument_hint
                .as_deref()
                .map(|h| format!(" {}", h))
                .unwrap_or_default();

            if is_selected {
                Line::from(vec![
                    Span::styled(
                        format!(" {}", entry.name),
                        Style::default()
                            .fg(theme::text_on_accent())
                            .bg(name_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        hint_str.clone(),
                        Style::default()
                            .fg(theme::text_on_accent())
                            .bg(name_color),
                    ),
                    Span::styled(
                        format!("  {} ", entry.description),
                        Style::default().fg(theme::text_on_accent()).bg(name_color),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(
                        format!(" {}", entry.name),
                        Style::default().fg(name_color),
                    ),
                    Span::styled(
                        hint_str,
                        Style::default().fg(theme::warning()),
                    ),
                    Span::styled(
                        format!("  {}", entry.description),
                        Style::default().fg(theme::text_muted()),
                    ),
                ])
            }
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::border()))
        .title(Span::styled(
            " Commands ",
            Style::default().fg(theme::text_muted()),
        ));

    // Clear the area first (so menu floats above content)
    frame.render_widget(Clear, area);

    let paragraph = Paragraph::new(lines)
        .block(block)
        .style(Style::default().bg(theme::bg_elevated()));

    frame.render_widget(paragraph, area);
}
