use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use super::theme;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let dir = shorten_path(&app.working_dir.to_string_lossy());
    let model = app.model_name.as_str();
    let sep = Span::styled(" │ ", Style::default().fg(theme::STATUS_SEP));

    let mut left: Vec<Span> = Vec::new();
    let mut right: Vec<Span> = Vec::new();

    // Left: brand + path (clean, no session title)
    left.push(Span::styled(
        format!(" atomcode ", ),
        Style::default().fg(theme::BRAND_FG).bg(theme::BRAND_BG).add_modifier(Modifier::BOLD),
    ));
    left.push(Span::styled(" ", Style::default()));
    left.push(Span::styled(&dir, Style::default().fg(theme::STATUS_PATH)));

    let is_active = app.turn_start.is_some();

    // Right side: turn state + model
    if is_active {
        // ── Active turn: spinner + turn N + live timer ──
        let spin = SPINNER[app.tick_count % SPINNER.len()];
        let secs = app.turn_start.unwrap().elapsed().as_secs();
        let dur = if secs >= 60 { format!("{}m{}s", secs / 60, secs % 60) } else { format!("{}s", secs) };

        let time_color = if secs < 10 {
            theme::WAIT_FAST
        } else if secs < 60 {
            theme::WAIT_NORMAL
        } else if secs < 120 {
            theme::WAIT_SLOW
        } else {
            theme::WAIT_VERY_SLOW
        };

        right.push(Span::styled(
            format!("{} ", spin),
            Style::default().fg(time_color),
        ));
        if app.current_step_count > 0 {
            right.push(Span::styled(
                format!("turn {}", app.current_step_count),
                Style::default().fg(theme::TEXT_SECONDARY),
            ));
            right.push(sep.clone());
        }
        right.push(Span::styled(
            dur,
            Style::default().fg(time_color).add_modifier(Modifier::BOLD),
        ));
        right.push(sep.clone());
    } else if app.last_turn_duration.is_some() || app.current_step_count > 0 {
        // ── Completed turn: ✓ + turn N + duration (green) ──
        right.push(Span::styled(
            "✓ ",
            Style::default().fg(theme::SUCCESS),
        ));
        if app.current_step_count > 0 {
            right.push(Span::styled(
                format!("turn {}", app.current_step_count),
                Style::default().fg(theme::TEXT_SECONDARY),
            ));
            right.push(sep.clone());
        }
        if let Some(dur) = app.last_turn_duration {
            let secs = dur.as_secs();
            let dur_str = if secs >= 60 { format!("{}m{}s", secs / 60, secs % 60) } else { format!("{}s", secs) };
            right.push(Span::styled(
                dur_str,
                Style::default().fg(theme::SUCCESS),
            ));
            right.push(sep.clone());
        }
    }

    // Context usage (show after first LLM call)
    if app.ctx_used_tokens > 0 && app.context_window > 0 {
        let used_k = app.ctx_used_tokens / 1000;
        let total_k = app.context_window / 1000;
        let ratio = app.ctx_used_tokens as f64 / app.context_window as f64;
        let ctx_color = if ratio < 0.5 {
            theme::SUCCESS
        } else if ratio < 0.8 {
            theme::WAIT_NORMAL
        } else {
            theme::WAIT_VERY_SLOW
        };
        right.push(Span::styled(
            format!("ctx {}K/{}K", used_k, total_k),
            Style::default().fg(ctx_color),
        ));
        right.push(sep.clone());
    }

    // Model name (always)
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

