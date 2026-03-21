use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::app::InputState;

const H_PADDING: u16 = 3;

use crate::file_attach::AttachedFile;

pub fn render(frame: &mut Frame, area: Rect, input: &InputState, is_busy: bool, suggestion: Option<&str>, attached: &[AttachedFile], pasted: Option<&str>) {
    let is_empty = input.is_empty() && pasted.is_none();

    // Always show user's input — even during streaming
    let lines: Vec<Line> = if let Some(pasted_text) = pasted {
        // Show pasted text indicator (like Claude Code)
        let line_count = pasted_text.lines().count();
        let char_count = pasted_text.len();
        let first_line = pasted_text.lines().next().unwrap_or("");
        let preview = if first_line.chars().count() > 50 {
            format!("{}...", first_line.chars().take(47).collect::<String>())
        } else {
            first_line.to_string()
        };

        let mut v = Vec::new();
        // Show typed text above the paste indicator
        if !input.is_empty() {
            for l in &input.lines {
                v.push(Line::from(Span::raw(l.clone())));
            }
        }
        // Pasted block indicator
        v.push(Line::from(vec![
            Span::styled(
                format!(" {} ", preview),
                Style::default().fg(Color::Rgb(180, 190, 200)).bg(Color::Rgb(35, 40, 50)),
            ),
        ]));
        v.push(Line::from(vec![
            Span::styled(
                format!(" {} lines, {} chars — pasted ", line_count, char_count),
                Style::default().fg(Color::Rgb(100, 110, 120)).bg(Color::Rgb(30, 33, 40)),
            ),
        ]));
        v
    } else if is_empty {
        if let Some(sug) = suggestion {
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
        // If more than 5 lines, show a scrolled view centered on the cursor row
        let max_visible = 5;
        if input.lines.len() > max_visible {
            let half = max_visible / 2;
            let start = if input.cursor_row <= half {
                0
            } else if input.cursor_row + half >= input.lines.len() {
                input.lines.len().saturating_sub(max_visible)
            } else {
                input.cursor_row - half
            };
            let end = (start + max_visible).min(input.lines.len());

            let mut lines_vec: Vec<Line> = Vec::new();
            if start > 0 {
                lines_vec.push(Line::from(Span::styled(
                    format!("  ↑ {} more lines", start),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            for i in start..end {
                lines_vec.push(Line::from(Span::raw(input.lines[i].clone())));
            }
            if end < input.lines.len() {
                lines_vec.push(Line::from(Span::styled(
                    format!("  ↓ {} more lines", input.lines.len() - end),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            lines_vec
        } else {
            input
                .lines
                .iter()
                .map(|l| Line::from(Span::raw(l.clone())))
                .collect()
        }
    };

    // Subtle border color change when busy
    let border_color = if is_busy {
        Color::Rgb(80, 80, 60)
    } else {
        Color::Rgb(100, 100, 100)
    };

    let prompt = Span::styled(
        " > ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );

    let hint = Span::styled(
        " Enter send · Shift+Enter newline · / commands · Ctrl+L clear ",
        Style::default().fg(Color::Rgb(55, 55, 60)),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(border_color))
        .title(prompt)
        .title_bottom(hint)
        .padding(Padding::horizontal(H_PADDING));

    // Render attached file tags above the input box
    let (input_area, _tag_offset) = if !attached.is_empty() {
        // Draw tags in the first line of the area
        let tag_area = Rect::new(area.x, area.y, area.width, 1);
        let mut tag_spans: Vec<Span> = vec![Span::raw(" ".to_string())];
        for file in attached {
            tag_spans.push(Span::styled(
                format!(" {} ", file.file_type),
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(60, 55, 80)),
            ));
            tag_spans.push(Span::styled(
                format!(" {} ", file.filename),
                Style::default().fg(Color::Rgb(150, 150, 160)),
            ));
            tag_spans.push(Span::raw("  ".to_string()));
        }
        let tag_line = Paragraph::new(Line::from(tag_spans));
        frame.render_widget(tag_line, tag_area);

        // Shrink input area
        let remaining = Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(1));
        (remaining, 1u16)
    } else {
        (area, 0u16)
    };

    let input_widget = Paragraph::new(lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .style(Style::default().fg(Color::White));

    frame.render_widget(input_widget, input_area);

    // Always show cursor (offset for tags and input area + scroll)
    let current_line = &input.lines[input.cursor_row];
    let safe_col = if input.cursor_col >= current_line.len() {
        current_line.len()
    } else if current_line.is_char_boundary(input.cursor_col) {
        input.cursor_col
    } else {
        current_line[..input.cursor_col]
            .char_indices()
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0)
    };
    let text_before_cursor = &current_line[..safe_col];
    let display_col = unicode_display_width(text_before_cursor);

    // Calculate visible row offset when scrolling
    let max_visible = 5;
    let visible_row = if input.lines.len() > max_visible {
        let half = max_visible / 2;
        let start = if input.cursor_row <= half {
            0
        } else if input.cursor_row + half >= input.lines.len() {
            input.lines.len().saturating_sub(max_visible)
        } else {
            input.cursor_row - half
        };
        let offset_in_view = input.cursor_row - start;
        // Account for the "↑ N more lines" indicator taking 1 line
        if start > 0 { offset_in_view + 1 } else { offset_in_view }
    } else {
        input.cursor_row
    };

    let cursor_x = input_area.x + 1 + H_PADDING + display_col as u16;
    let cursor_y = input_area.y + 1 + visible_row as u16;
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn unicode_display_width(s: &str) -> usize {
    s.chars()
        .map(|c| if is_wide_char(c) { 2 } else { 1 })
        .sum()
}

fn is_wide_char(c: char) -> bool {
    let cp = c as u32;
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x20000..=0x2A6DF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFF01..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0xAC00..=0xD7AF).contains(&cp)
        || (0x3000..=0x303F).contains(&cp)
        || (0x3040..=0x309F).contains(&cp)
        || (0x30A0..=0x30FF).contains(&cp)
        || (0x3200..=0x32FF).contains(&cp)
        || (0x3300..=0x33FF).contains(&cp)
}

pub fn height(input: &InputState, terminal_height: u16, has_attachments: bool) -> u16 {
    let content_lines = input.lines.len() as u16;
    let tag_height = if has_attachments { 1 } else { 0 };
    let min_height = 3 + tag_height;
    // Cap input box at 5 lines of content (like Claude Code) — prevents the input
    // from taking over the screen. Users can still type more, it just scrolls.
    let max_content = 5;
    let max_height = (max_content + 2 + tag_height).min(terminal_height / 2);
    (content_lines + 2 + tag_height).clamp(min_height, max_height)
}
