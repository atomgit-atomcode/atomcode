//! Session selector inline UI.
//! Displays session list in the chat area for /resume command.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use atomcode_core::session::SessionMeta;
use super::theme;

/// Format file size in human-readable format (KB, MB, etc.)
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format timestamp as human-readable string
fn format_timestamp(ts: u64) -> String {
    use chrono::{TimeZone, Utc, Local};
    let dt = Utc.timestamp_opt(ts as i64, 0).single().unwrap_or_else(|| Utc::now());
    let local = dt.with_timezone(&Local);
    let now = Local::now();
    
    let duration = now.signed_duration_since(local);
    if duration.num_minutes() < 1 {
        "just now".to_string()
    } else if duration.num_hours() < 1 {
        format!("{}m ago", duration.num_minutes())
    } else if duration.num_hours() < 24 {
        format!("{}h ago", duration.num_hours())
    } else if duration.num_days() < 7 {
        format!("{}d ago", duration.num_days())
    } else {
        local.format("%m-%d %H:%M").to_string()
    }
}
/// Render the session selector inline in the chat area.
/// Returns the number of lines rendered (for scroll calculation).
/// selected = 0 means search box is focused, 1+ means session item is selected.
pub fn render(frame: &mut Frame, area: Rect, sessions: &[SessionMeta], selected: usize, query: &str, cursor_pos: usize) -> usize {
    // Filter sessions by query
    let filtered: Vec<(usize, &SessionMeta)> = sessions.iter().enumerate()
        .filter(|(_, s)| query.is_empty() 
            || s.name.to_lowercase().contains(&query.to_lowercase()))
        .collect();
    
    let mut lines: Vec<Line> = Vec::new();
    
    // Header
    lines.push(Line::from(vec![
        Span::styled(" Resume Session", Style::default().fg(theme::brand_fg()).add_modifier(Modifier::BOLD)),
    ]));
    
    lines.push(Line::default()); // blank line
    
    // Search row - first item (index 0)
    let search_is_selected = selected == 0;
    let search_bg = if search_is_selected { theme::brand_bg() } else { theme::bg_surface() };
    let search_fg = if search_is_selected { theme::text_primary() } else { theme::text_secondary() };
    
    // Search row with box style
    let search_indicator = if search_is_selected { " \u{25b8} " } else { "   " };
    let text_style = Style::default().fg(search_fg).bg(search_bg);
    let cursor_style = Style::default().fg(search_fg).bg(search_bg);

    if query.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(search_indicator, text_style),
            Span::styled("Search: ", text_style),
            Span::styled("\u{2588}", cursor_style),
            Span::styled(" Type to search...", Style::default().fg(theme::text_muted()).bg(search_bg)),
        ]));
    } else {
        let (before, after) = query.split_at(cursor_pos.min(query.len()));
        lines.push(Line::from(vec![
            Span::styled(search_indicator, text_style),
            Span::styled("Search: ", text_style),
            Span::styled(before.to_string(), text_style),
            Span::styled("\u{2588}", cursor_style),
            Span::styled(after.to_string(), text_style),
            Span::styled(" ", Style::default().bg(search_bg)),
        ]));
    }
    
    // Cursor hint when search is selected
    if search_is_selected {
        lines.push(Line::from(vec![
            Span::styled("      ", Style::default()),
            Span::styled("Type to filter sessions", Style::default().fg(theme::text_muted())),
        ]));
    }
    
    lines.push(Line::default()); // blank line
    
    if filtered.is_empty() {
        if query.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  No sessions found. Start a conversation first.", Style::default().fg(theme::text_muted())),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  No sessions matching '", Style::default().fg(theme::text_muted())),
                Span::styled(query, Style::default().fg(theme::text_secondary())),
                Span::styled("'", Style::default().fg(theme::text_muted())),
            ]));
        }
    } else {
        for (idx, (_, session)) in filtered.iter().enumerate() {
            // Item index is 1 + idx (0 is search row)
            let item_index = 1 + idx;
            let is_selected = item_index == selected;
            
            // Line 1: Session name with indicator
            let indicator = if is_selected { " \u{25b8} " } else { "   " };
            let name_style = if is_selected {
                Style::default().fg(theme::text_on_accent()).bg(theme::brand_bg()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::text_primary())
            };
            
            lines.push(Line::from(vec![
                Span::styled(indicator, name_style),
                Span::styled(&session.name, name_style),
            ]));
            
            // Line 2: Time and size (dimmed, indented)
            let time_str = format_timestamp(session.updated_at);
            let size_str = format_size(session.file_size);
            let msg_str = format!("{} messages", session.message_count);
            let meta_style = if is_selected {
                Style::default().fg(theme::text_secondary())
            } else {
                Style::default().fg(theme::text_muted())
            };
            
            lines.push(Line::from(vec![
                Span::styled("      ", Style::default()),
                Span::styled(time_str, meta_style),
                Span::styled("  \u{2022}  ", meta_style),
                Span::styled(size_str, meta_style),
                Span::styled("  \u{2022}  ", meta_style),
                Span::styled(msg_str, meta_style),
            ]));
            
            lines.push(Line::default()); // blank line between sessions
        }
    }
    
    // Help hint
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled("\u{2191}\u{2193}", Style::default().fg(theme::text_muted())),
        Span::styled(" navigate  ", Style::default().fg(theme::text_muted())),
        Span::styled("type", Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)),
        Span::styled(" to search  ", Style::default().fg(theme::text_muted())),
        Span::styled("Enter", Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)),
        Span::styled(" resume  ", Style::default().fg(theme::text_muted())),
        Span::styled("Esc", Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)),
        Span::styled(" cancel", Style::default().fg(theme::text_muted())),
    ]));
    
    // Calculate scroll offset to keep selected item visible
    let total_lines = lines.len();
    let visible_height = area.height as usize;
    
    // Each session item occupies 3 lines (name + meta + blank)
    // Search row starts at line 4 (header + blank + search + hint? + blank)
    // Calculate which line the selected item starts on
    let selected_line = if selected == 0 {
        // Search row is at line 2 (0-indexed: header=0, blank=1, search=2)
        2
    } else {
        // Session items start at line 5 or 6 depending on search hint
        // Header = 1, blank = 1, search = 1, hint (if search selected) = 1, blank = 1
        let header_lines = if search_is_selected { 6 } else { 5 };
        // Each session item = 3 lines
        header_lines + (selected - 1) * 3
    };
    
    // Calculate scroll offset
    let scroll_offset = if selected_line >= visible_height {
        // Scroll to keep selected item visible, with some context above
        selected_line.saturating_sub(visible_height.saturating_sub(5))
    } else {
        0
    };
    
    // Apply scroll offset
    let visible_lines: Vec<Line> = lines.into_iter()
        .skip(scroll_offset)
        .take(visible_height)
        .collect();
    
    let paragraph = Paragraph::new(visible_lines);
    frame.render_widget(paragraph, area);
    
    // Return total line count for scroll calculation
    total_lines
}
