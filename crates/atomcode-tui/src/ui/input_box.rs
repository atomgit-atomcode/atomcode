use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::app::InputState;

const H_PADDING: u16 = 3;

use crate::file_attach::AttachedFile;

/// Render the input box. Grows dynamically with content, up to half the terminal height.
/// Scrolls to keep the cursor visible when content exceeds the visible area.
pub fn render(frame: &mut Frame, area: Rect, input: &InputState, is_busy: bool, suggestion: Option<&str>, attached: &[AttachedFile]) {
    let is_empty = input.is_empty();

    // Build visible lines with scroll tracking
    let max_visible = (area.height as usize).saturating_sub(2); // minus borders

    let (lines, visible_row) = if is_empty {
        let placeholder = if let Some(sug) = suggestion {
            vec![Line::from(vec![
                Span::styled(sug.to_string(), Style::default().fg(Color::Rgb(70, 70, 70))),
                Span::styled("  Tab", Style::default().fg(Color::Rgb(50, 50, 50))),
            ])]
        } else {
            vec![Line::from(Span::styled(
                "Ask anything... (Enter to send, / for commands)",
                Style::default().fg(Color::DarkGray),
            ))]
        };
        (placeholder, 0usize)
    } else if input.lines.len() <= max_visible {
        // All lines fit — show everything
        let lines: Vec<Line> = input.lines.iter()
            .map(|l| Line::from(Span::raw(l.clone())))
            .collect();
        (lines, input.cursor_row)
    } else {
        // Scroll: center on cursor row
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
                format!("  \u{2191} {} more", start),
                Style::default().fg(Color::Rgb(60, 65, 75)),
            )));
        }
        for i in start..end {
            lines_vec.push(Line::from(Span::raw(input.lines[i].clone())));
        }
        if end < input.lines.len() {
            lines_vec.push(Line::from(Span::styled(
                format!("  \u{2193} {} more", input.lines.len() - end),
                Style::default().fg(Color::Rgb(60, 65, 75)),
            )));
        }

        let offset_in_view = input.cursor_row - start;
        let vis_row = if start > 0 { offset_in_view + 1 } else { offset_in_view };
        (lines_vec, vis_row)
    };

    let border_color = if is_busy { Color::Rgb(80, 80, 60) } else { Color::Rgb(100, 100, 100) };

    let prompt = Span::styled(" > ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(border_color))
        .title(prompt)
        .padding(Padding::horizontal(H_PADDING));

    // Render attached file tags
    let (input_area, _tag_offset) = if !attached.is_empty() {
        let tag_area = Rect::new(area.x, area.y, area.width, 1);
        let mut tag_spans: Vec<Span> = vec![Span::raw(" ".to_string())];
        for file in attached {
            tag_spans.push(Span::styled(
                format!(" {} ", file.file_type),
                Style::default().fg(Color::White).bg(Color::Rgb(60, 55, 80)),
            ));
            tag_spans.push(Span::styled(
                format!(" {} ", file.filename),
                Style::default().fg(Color::Rgb(150, 150, 160)),
            ));
            tag_spans.push(Span::raw("  ".to_string()));
        }
        frame.render_widget(Paragraph::new(Line::from(tag_spans)), tag_area);
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

    // Cursor — account for line wrapping
    let inner_width = input_area.width.saturating_sub(2 + H_PADDING * 2) as usize;
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

    // Account for visual line wrapping: a long line wraps into multiple visual rows.
    // cursor_x = display_col % inner_width (position within the wrapped line)
    // extra_rows = display_col / inner_width (how many wrapped rows above)
    let (cursor_col_in_wrap, wrap_extra_rows) = if inner_width > 0 {
        (display_col % inner_width, display_col / inner_width)
    } else {
        (display_col, 0)
    };

    // Also count wrapped rows from PREVIOUS lines (lines before cursor_row)
    let mut prev_wrap_rows = 0usize;
    if !is_empty {
        let start_line = if input.lines.len() > (area.height as usize).saturating_sub(2) {
            // In scroll mode — count from scroll start
            let max_vis = (area.height as usize).saturating_sub(2);
            let half = max_vis / 2;
            if input.cursor_row <= half { 0 }
            else if input.cursor_row + half >= input.lines.len() {
                input.lines.len().saturating_sub(max_vis)
            } else { input.cursor_row - half }
        } else { 0 };

        for i in start_line..input.cursor_row.min(input.lines.len()) {
            let line_width = unicode_display_width(&input.lines[i]);
            if inner_width > 0 && line_width > inner_width {
                prev_wrap_rows += line_width / inner_width;
            }
        }
    }

    let cursor_x = input_area.x + 1 + H_PADDING + cursor_col_in_wrap as u16;
    let total_row = visible_row + wrap_extra_rows + prev_wrap_rows;
    let cursor_y = input_area.y + 1 + total_row as u16;

    // Clamp to input area bounds
    let max_y = input_area.y + input_area.height.saturating_sub(2);
    frame.set_cursor_position((cursor_x, cursor_y.min(max_y)));
}

fn unicode_display_width(s: &str) -> usize {
    s.chars().map(|c| if is_wide_char(c) { 2 } else { 1 }).sum()
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

/// Input box height: grows with content, max half terminal height.
pub fn height(input: &InputState, terminal_height: u16, terminal_width: u16, has_attachments: bool) -> u16 {
    let tag_height = if has_attachments { 1 } else { 0 };
    let min_height = 3 + tag_height;
    let max_content: u16 = 10;
    let max_height = (max_content + 2 + tag_height).min(terminal_height / 2);

    // Inner width: terminal width - 2 borders - 2*H_PADDING
    let inner_width = terminal_width.saturating_sub(2 + H_PADDING * 2) as usize;
    let inner_width = inner_width.max(1);

    // Count VISUAL lines (including word wrap)
    let visual_lines: u16 = input.lines.iter().map(|line| {
        let w = unicode_display_width_line(line);
        if w > inner_width {
            ((w + inner_width - 1) / inner_width) as u16
        } else {
            1
        }
    }).sum();

    (visual_lines + 2 + tag_height).clamp(min_height, max_height)
}

fn unicode_display_width_line(s: &str) -> usize {
    s.chars().map(|c| if is_wide_char(c) { 2 } else { 1 }).sum()
}
