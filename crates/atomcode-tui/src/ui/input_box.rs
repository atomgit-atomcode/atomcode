use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::app::InputState;

/// Left/right inner padding for the input box content.
const H_PADDING: u16 = 3;

pub fn render(frame: &mut Frame, area: Rect, input: &InputState, is_streaming: bool, suggestion: Option<&str>) {
    let is_empty = input.is_empty();

    // Build content lines
    let lines: Vec<Line> = if is_empty && !is_streaming {
        if let Some(sug) = suggestion {
            // Show suggestion as ghost text with Tab hint
            vec![Line::from(vec![
                Span::styled(
                    sug.to_string(),
                    Style::default().fg(Color::Rgb(70, 70, 70)),
                ),
                Span::styled(
                    "  Tab",
                    Style::default().fg(Color::Rgb(50, 50, 50)),
                ),
            ])]
        } else {
            vec![Line::from(Span::styled(
                "Ask anything... (Enter to send, / for commands)",
                Style::default().fg(Color::DarkGray),
            ))]
        }
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
    //   x = area.x + 1 (left border) + H_PADDING + display_width_of_text_before_cursor
    //   y = area.y + 1 (top border) + cursor_row
    // cursor_col is a byte index; we need the display width (columns) of the text before it.
    if !is_streaming {
        let current_line = &input.lines[input.cursor_row];
        let text_before_cursor = &current_line[..input.cursor_col];
        let display_col = unicode_display_width(text_before_cursor);
        let cursor_x = area.x + 1 + H_PADDING + display_col as u16;
        let cursor_y = area.y + 1 + input.cursor_row as u16;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Calculate the display width (terminal columns) of a string.
/// CJK characters take 2 columns, ASCII takes 1.
fn unicode_display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if is_wide_char(c) {
                2
            } else {
                1
            }
        })
        .sum()
}

/// Check if a character is a wide (CJK) character that takes 2 terminal columns.
fn is_wide_char(c: char) -> bool {
    let cp = c as u32;
    // CJK Unified Ideographs
    (0x4E00..=0x9FFF).contains(&cp)
        // CJK Extension A
        || (0x3400..=0x4DBF).contains(&cp)
        // CJK Extension B+
        || (0x20000..=0x2A6DF).contains(&cp)
        // CJK Compatibility Ideographs
        || (0xF900..=0xFAFF).contains(&cp)
        // Fullwidth Forms
        || (0xFF01..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        // Hangul
        || (0xAC00..=0xD7AF).contains(&cp)
        // Katakana / Hiragana
        || (0x3000..=0x303F).contains(&cp)
        || (0x3040..=0x309F).contains(&cp)
        || (0x30A0..=0x30FF).contains(&cp)
        // Enclosed CJK
        || (0x3200..=0x32FF).contains(&cp)
        || (0x3300..=0x33FF).contains(&cp)
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
