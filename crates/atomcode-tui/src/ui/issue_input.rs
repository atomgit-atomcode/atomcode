use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme;
use crate::app::{IssueField, IssueInputState};

pub fn render(frame: &mut Frame, area: Rect, state: &IssueInputState) {
    // Clear the entire chat area first to prevent artifacts when switching modes
    frame.render_widget(Clear, area);

    // Create centered popup
    let height = 14u16;
    let width = 70u16.min(area.width.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::accent()))
        .title(Span::styled(
            " Create Issue on AtomGit ",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme::bg_elevated()));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // Layout: title label, title input, desc label, desc input, help
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // spacing
            Constraint::Length(1), // title label
            Constraint::Length(2), // title input
            Constraint::Length(1), // spacing
            Constraint::Length(1), // desc label
            Constraint::Length(3), // desc input (multiline visual)
            Constraint::Length(1), // spacing
            Constraint::Length(1), // help/error
        ])
        .split(inner);

    // Title label
    let title_label = Paragraph::new(Line::from(vec![
        Span::styled("Title", Style::default().fg(theme::text_primary())),
        Span::styled(" (required)", Style::default().fg(theme::text_muted())),
    ]));
    frame.render_widget(title_label, chunks[1]);

    // Title input box - use accent (light purple) for focus, with dark text for contrast
    let title_style = if state.cursor_field == IssueField::Title {
        Style::default()
            .fg(theme::text_on_accent())
            .bg(theme::accent())
    } else {
        Style::default().fg(theme::text_primary())
    };
    let title_block = Block::default().borders(Borders::BOTTOM);
    let title_text = if state.cursor_field == IssueField::Title {
        // Show cursor at current position with horizontal scroll
        let available_width = chunks[2].width.saturating_sub(2) as usize;
        let cursor_pos = state.title_cursor;
        let _char_count = state.title.chars().count();

        // Calculate scroll offset to keep cursor visible
        let scroll_offset = if cursor_pos >= available_width {
            cursor_pos - available_width + 1
        } else {
            0
        };

        let chars: Vec<char> = state
            .title
            .chars()
            .skip(scroll_offset)
            .take(available_width)
            .collect();
        let display: String = chars.iter().collect();

        // Insert cursor at the right position
        let cursor_idx = cursor_pos.saturating_sub(scroll_offset);
        if cursor_idx < display.len() {
            let mut result: String = display.chars().take(cursor_idx).collect();
            result.push('_');
            result.extend(display.chars().skip(cursor_idx));
            result
        } else if display.len() < available_width {
            format!("{}_", display)
        } else {
            // Cursor at end after scrolling
            format!("{}_", display)
        }
    } else if state.title.is_empty() {
        " ".to_string()
    } else {
        state.title.clone()
    };
    let title_input = Paragraph::new(Span::styled(title_text, title_style)).block(title_block);
    frame.render_widget(title_input, chunks[2]);

    // Description label
    let desc_label = Paragraph::new(Line::from(vec![
        Span::styled("Description", Style::default().fg(theme::text_primary())),
        Span::styled(" (required)", Style::default().fg(theme::text_muted())),
    ]));
    frame.render_widget(desc_label, chunks[4]);

    // Description input box - use accent (light purple) for focus, with dark text for contrast
    let desc_style = if state.cursor_field == IssueField::Description {
        Style::default()
            .fg(theme::text_on_accent())
            .bg(theme::accent())
    } else {
        Style::default().fg(theme::text_primary())
    };
    let desc_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(theme::border()));
    let desc_text = if state.cursor_field == IssueField::Description {
        // Show cursor at current position with horizontal scroll
        let available_width = chunks[5].width.saturating_sub(2) as usize;
        let cursor_pos = state.desc_cursor;
        let _char_count = state.description.chars().count();

        // Calculate scroll offset to keep cursor visible
        let scroll_offset = if cursor_pos >= available_width {
            cursor_pos - available_width + 1
        } else {
            0
        };

        let chars: Vec<char> = state
            .description
            .chars()
            .skip(scroll_offset)
            .take(available_width)
            .collect();
        let display: String = chars.iter().collect();

        // Insert cursor at the right position
        let cursor_idx = cursor_pos.saturating_sub(scroll_offset);
        if cursor_idx < display.len() {
            let mut result: String = display.chars().take(cursor_idx).collect();
            result.push('_');
            result.extend(display.chars().skip(cursor_idx));
            result
        } else if display.len() < available_width {
            format!("{}_", display)
        } else {
            format!("{}_", display)
        }
    } else if state.description.is_empty() {
        " ".to_string()
    } else {
        state.description.clone()
    };
    let desc_input = Paragraph::new(Span::styled(desc_text, desc_style)).block(desc_block);
    frame.render_widget(desc_input, chunks[5]);

    // Help/error line
    let help_line = if let Some(ref err) = state.error {
        Line::from(Span::styled(
            format!(" Error: {} ", err),
            Style::default().fg(theme::error()),
        ))
    } else if state.submitting {
        Line::from(Span::styled(
            " Submitting...",
            Style::default().fg(theme::info()),
        ))
    } else {
        Line::from(vec![
            Span::styled("←→", Style::default().fg(theme::accent())),
            Span::styled(" move  ", Style::default().fg(theme::text_muted())),
            Span::styled("Tab", Style::default().fg(theme::accent())),
            Span::styled(" switch  ", Style::default().fg(theme::text_muted())),
            Span::styled("Enter", Style::default().fg(theme::accent())),
            Span::styled(" submit  ", Style::default().fg(theme::text_muted())),
            Span::styled("Esc", Style::default().fg(theme::accent())),
            Span::styled(" cancel", Style::default().fg(theme::text_muted())),
        ])
    };
    let help = Paragraph::new(help_line);
    frame.render_widget(help, chunks[7]);
}
