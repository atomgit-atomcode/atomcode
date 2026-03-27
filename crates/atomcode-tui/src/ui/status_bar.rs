use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use super::theme;

/// Minimal status bar: brand │ path │ model │ turn N
/// No mode badges, timers, or speed — those are in the chat spinner.
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let dir = shorten_path(&app.working_dir.to_string_lossy());
    let model = app.model_name.as_str();
    let sep = Span::styled(" \u{2502} ", Style::default().fg(theme::STATUS_SEP));

    let mut left: Vec<Span> = Vec::new();
    let mut right: Vec<Span> = Vec::new();

    // Left: brand + path
    left.push(Span::styled(
        " atomcode ",
        Style::default().fg(theme::BRAND_FG).bg(theme::BRAND_BG).add_modifier(Modifier::BOLD),
    ));
    left.push(Span::styled(" ", Style::default()));
    left.push(Span::styled(dir, Style::default().fg(theme::STATUS_PATH)));

    // Right: turn count (if active) │ model
    if app.current_step_count > 0 {
        right.push(Span::styled(
            format!("turn {}", app.current_step_count),
            Style::default().fg(theme::TEXT_SECONDARY),
        ));
        right.push(sep.clone());
    }

    // Duration (active or last)
    if let Some(start) = app.turn_start {
        let secs = start.elapsed().as_secs();
        let dur = if secs >= 60 { format!("{}m{}s", secs / 60, secs % 60) } else { format!("{}s", secs) };
        right.push(Span::styled(dur, Style::default().fg(theme::WARNING)));
        right.push(sep.clone());
    } else if let Some(dur) = app.last_turn_duration {
        let secs = dur.as_secs();
        let dur_str = if secs >= 60 { format!("{}m{}s", secs / 60, secs % 60) } else { format!("{}s", secs) };
        right.push(Span::styled(dur_str, Style::default().fg(theme::SUCCESS)));
        right.push(sep.clone());
    }

    // Model name
    right.push(Span::styled(
        format!("{} ", model),
        Style::default().fg(theme::STATUS_MODEL).add_modifier(Modifier::BOLD),
    ));

    // Layout
    let left_w: usize = left.iter().map(|s| display_width(&s.content)).sum();
    let right_w: usize = right.iter().map(|s| display_width(&s.content)).sum();
    let pad = width.saturating_sub(left_w + right_w);

    let mut all = left;
    all.push(Span::styled(" ".repeat(pad), Style::default()));
    all.extend(right);

    let bar = Paragraph::new(Line::from(all))
        .style(Style::default().bg(theme::BG_SURFACE));
    frame.render_widget(bar, area);
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| {
        let cp = c as u32;
        if (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp)
            || (0x20000..=0x2A6DF).contains(&cp) || (0xF900..=0xFAFF).contains(&cp)
            || (0xFF01..=0xFF60).contains(&cp) || (0xFFE0..=0xFFE6).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp) || (0x3000..=0x303F).contains(&cp)
            || (0x3040..=0x309F).contains(&cp) || (0x30A0..=0x30FF).contains(&cp)
        { 2 } else { 1 }
    }).sum()
}

fn shorten_path(path: &str) -> String {
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    let shortened = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    if shortened.len() > 50 {
        let parts: Vec<&str> = shortened.rsplitn(3, '/').collect();
        if parts.len() >= 3 {
            format!(".../{}/{}", parts[1], parts[0])
        } else {
            shortened
        }
    } else {
        shortened
    }
}
