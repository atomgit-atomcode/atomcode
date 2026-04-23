use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::app::InputState;
use crate::ui::theme;

const H_PADDING: u16 = 3;

use crate::file_attach::AttachedFile;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    input: &InputState,
    is_busy: bool,
    attached: &[AttachedFile],
    pasted: &[String],
) {
    let is_empty = input.is_empty() && pasted.is_empty();
    let inner_width = area.width.saturating_sub(2 + H_PADDING * 2) as usize;
    let inner_width = inner_width.max(1);

    // Build visual lines by manually wrapping each logical line.
    let (visual_lines, cursor_visual_row, cursor_visual_col) = if is_empty {
        (vec![Line::from(Span::raw(String::new()))], 0usize, 0usize)
    } else {
        let mut vis_lines: Vec<Line<'static>> = Vec::new();
        let mut cursor_vrow = 0usize;
        let mut cursor_vcol = 0usize;

        // Pasted text: one compact tag per staged paste block. Multiple pastes
        // are preserved (see App::pasted_blocks); each shows on its own line so
        // the user can see "I pasted 3 blocks" at a glance.
        for (i, pt) in pasted.iter().enumerate() {
            let line_count = pt.lines().count();
            let char_count = pt.len();
            let label = if pasted.len() > 1 {
                format!("\u{25a0} Pasted text #{} ", i + 1)
            } else {
                "\u{25a0} Pasted text ".to_string()
            };
            vis_lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(theme::info())),
                Span::styled(
                    format!("({} lines, {} chars)", line_count, char_count),
                    Style::default().fg(theme::text_muted()),
                ),
            ]));
        }

        for (logical_row, line) in input.lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let display_widths: Vec<usize> = chars
                .iter()
                .map(|c| if is_wide_char(*c) { 2 } else { 1 })
                .collect();

            if chars.is_empty() {
                // Empty line
                let vrow = vis_lines.len();
                vis_lines.push(Line::from(Span::raw(String::new())));
                if logical_row == input.cursor_row {
                    cursor_vrow = vrow;
                    cursor_vcol = 0;
                }
                continue;
            }

            // Split this logical line into visual lines based on inner_width
            let mut col = 0usize;
            let mut visual_col = 0usize;
            let mut segment_start = 0usize;

            while col < chars.len() {
                let char_w = display_widths[col];
                if visual_col + char_w > inner_width && segment_start < col {
                    // Wrap: emit current segment as a visual line
                    let vrow = vis_lines.len();
                    let segment: String = chars[segment_start..col].iter().collect();
                    vis_lines.push(Line::from(Span::raw(segment)));

                    // Check if cursor is in this segment
                    if logical_row == input.cursor_row {
                        let cursor_char_idx = char_index_at_byte(line, input.cursor_col);
                        if cursor_char_idx >= segment_start && cursor_char_idx < col {
                            cursor_vrow = vrow;
                            cursor_vcol = display_width_range(
                                &display_widths,
                                segment_start,
                                cursor_char_idx,
                            );
                        }
                    }

                    segment_start = col;
                    visual_col = 0;
                }
                visual_col += char_w;
                col += 1;
            }

            // Last segment (or entire line if no wrap)
            let vrow = vis_lines.len();
            let segment: String = chars[segment_start..].iter().collect();
            vis_lines.push(Line::from(Span::raw(segment)));

            if logical_row == input.cursor_row {
                let cursor_char_idx = char_index_at_byte(line, input.cursor_col);
                if cursor_char_idx >= segment_start {
                    cursor_vrow = vrow;
                    cursor_vcol =
                        display_width_range(&display_widths, segment_start, cursor_char_idx);
                }
            }
        }

        (vis_lines, cursor_vrow, cursor_vcol)
    };

    // Scroll if too many visual lines
    let max_visible = area.height.saturating_sub(2) as usize;
    let (display_lines, scroll_offset) = if visual_lines.len() <= max_visible {
        (visual_lines.clone(), 0usize)
    } else {
        let half = max_visible / 2;
        let start = if cursor_visual_row <= half {
            0
        } else if cursor_visual_row + half >= visual_lines.len() {
            visual_lines.len().saturating_sub(max_visible)
        } else {
            cursor_visual_row - half
        };
        let end = (start + max_visible).min(visual_lines.len());
        (visual_lines[start..end].to_vec(), start)
    };
    // Focus state: idle (ready) = bright bold border to signal "you can type here";
    // busy (agent working) = dim muted border to signal "input is locked right now".
    let (border_color, border_bold) = if is_busy {
        (theme::text_muted(), false)
    } else {
        (theme::text_secondary(), true)
    };
    let mut border_style = Style::default().fg(border_color);
    if border_bold {
        border_style = border_style.add_modifier(Modifier::BOLD);
    }
    let prompt = Span::styled(
        " \u{276f} ",
        Style::default()
            .fg(theme::user_chevron())
            .add_modifier(Modifier::BOLD),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(prompt)
        .padding(Padding::horizontal(H_PADDING));

    // Attached file tags
    let (input_area, _) = if !attached.is_empty() {
        let tag_area = Rect::new(area.x, area.y, area.width, 1);
        let mut tag_spans: Vec<Span> = vec![Span::raw(" ".to_string())];
        for file in attached {
            tag_spans.push(Span::styled(
                format!(" {} ", file.file_type),
                Style::default()
                    .fg(theme::text_on_accent())
                    .bg(theme::brand_bg()),
            ));
            tag_spans.push(Span::styled(
                format!(" {} ", file.filename),
                Style::default().fg(theme::text_secondary()),
            ));
            tag_spans.push(Span::raw("  ".to_string()));
        }
        frame.render_widget(Paragraph::new(Line::from(tag_spans)), tag_area);
        (
            Rect::new(
                area.x,
                area.y + 1,
                area.width,
                area.height.saturating_sub(1),
            ),
            1u16,
        )
    } else {
        (area, 0u16)
    };
    // No Wrap — we already split lines manually
    let widget = Paragraph::new(display_lines)
        .block(block)
        .style(Style::default().fg(theme::text_primary()));
    frame.render_widget(widget, input_area);

    // Cursor position — exact because we track visual rows ourselves
    let cursor_row_in_view = cursor_visual_row.saturating_sub(scroll_offset);
    let cursor_x = input_area.x + 1 + H_PADDING + cursor_visual_col as u16;
    let cursor_y = input_area.y + 1 + cursor_row_in_view as u16;
    let max_y = input_area.y + input_area.height.saturating_sub(2);
    frame.set_cursor_position((cursor_x, cursor_y.min(max_y)));
}

/// Convert byte offset to char index
fn char_index_at_byte(s: &str, byte_pos: usize) -> usize {
    s.char_indices()
        .position(|(i, _)| i >= byte_pos)
        .unwrap_or_else(|| s.chars().count())
}

/// Sum display widths for chars[start..end]
fn display_width_range(widths: &[usize], start: usize, end: usize) -> usize {
    widths[start..end.min(widths.len())].iter().sum()
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

/// Input box height: counts visual lines (including manual wrapping).
pub fn height(
    input: &InputState,
    terminal_height: u16,
    terminal_width: u16,
    has_attachments: bool,
    paste_block_count: usize,
) -> u16 {
    let tag_height = if has_attachments { 1 } else { 0 };
    // One row per staged paste block so multiple pastes are all visible.
    let paste_height: u16 = paste_block_count.min(5) as u16;
    let min_height = 3 + tag_height;
    let max_content: u16 = 8;
    let max_height = (max_content + 2 + tag_height)
        .min(terminal_height / 2)
        .max(min_height);

    let inner_width = terminal_width.saturating_sub(2 + H_PADDING * 2) as usize;
    let inner_width = inner_width.max(1);

    let visual_lines: u16 = input
        .lines
        .iter()
        .map(|line| {
            let w: usize = line
                .chars()
                .map(|c| if is_wide_char(c) { 2 } else { 1 })
                .sum();
            if w > inner_width {
                ((w + inner_width - 1) / inner_width) as u16
            } else {
                1
            }
        })
        .sum();

    (visual_lines + paste_height + 2 + tag_height).clamp(min_height, max_height)
}
