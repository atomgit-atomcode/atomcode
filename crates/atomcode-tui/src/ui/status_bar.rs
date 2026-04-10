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
    let sep = Span::styled(" │ ", Style::default().fg(theme::status_sep()));

    let mut left: Vec<Span> = Vec::new();
    let mut right: Vec<Span> = Vec::new();

    // Left: brand (with build hash) + path + session
    left.push(Span::styled(
        " Atom",
        Style::default().fg(theme::brand_fg()).bg(theme::brand_bg()).add_modifier(Modifier::BOLD),
    ));
    left.push(Span::styled(
        "Code ",
        Style::default().fg(theme::accent()).bg(theme::brand_bg()).add_modifier(Modifier::BOLD),
    ));
    left.push(Span::styled(
        format!("[{}] ", env!("ATOMCODE_BUILD_ID")),
        Style::default().fg(theme::brand_fg()).bg(theme::brand_bg()),
    ));
    left.push(Span::styled(" ", Style::default()));
    left.push(Span::styled(&dir, Style::default().fg(theme::status_path())));

    // Calculate available space for session name
    // Right side content (will be computed below)
    let is_active = app.turn_start.is_some();
    let mut right_preview: Vec<String> = Vec::new();
    
    if is_active {
        let secs = app.turn_start.unwrap().elapsed().as_secs();
        let dur = if secs >= 60 { format!("{}m{}s", secs / 60, secs % 60) } else { format!("{}s", secs) };
        right_preview.push(format!("{} ", SPINNER[app.tick_count % SPINNER.len()]));
        if app.current_step_count > 0 {
            right_preview.push(format!("turn {}", app.current_step_count));
            right_preview.push(" │ ".to_string());
        }
        right_preview.push(dur);
        right_preview.push(" │ ".to_string());
    } else if app.last_turn_duration.is_some() || app.current_step_count > 0 {
        right_preview.push("✓ ".to_string());
        if app.current_step_count > 0 {
            right_preview.push(format!("turn {}", app.current_step_count));
            right_preview.push(" │ ".to_string());
        }
        if let Some(dur) = app.last_turn_duration {
            let secs = dur.as_secs();
            let dur_str = if secs >= 60 { format!("{}m{}s", secs / 60, secs % 60) } else { format!("{}s", secs) };
            right_preview.push(dur_str);
            right_preview.push(" │ ".to_string());
        }
    }
    if app.ctx_used_tokens > 0 && app.context_window > 0 {
        right_preview.push(format!("ctx {}K/{}K", app.ctx_used_tokens / 1000, app.context_window / 1000));
        right_preview.push(" │ ".to_string());
    }
    right_preview.push(format!("{} ", model));

    // Calculate session name max length
    let brand_w = 10; // " atomcode " + " "
    let dir_w = display_width(&dir);
    let session_brackets_w = 3; // " [" + "]"
    let right_w: usize = right_preview.iter().map(|s| display_width(s)).sum();
    let min_padding = 1; // at least one space between left and right
    
    let fixed_width = brand_w + dir_w + session_brackets_w + right_w + min_padding;
    let max_session_len = width.saturating_sub(fixed_width);
    // Allow up to 60 chars but at least 10
    let _max_session_len = max_session_len.min(60).max(10);
    
    // Session name removed — takes up space without adding value.

    // Right side: turn state + model
    if is_active {
        // ── Active turn: spinner + turn N + live timer ──
        let spin = SPINNER[app.tick_count % SPINNER.len()];
        let secs = app.turn_start.unwrap().elapsed().as_secs();
        let dur = if secs >= 60 { format!("{}m{}s", secs / 60, secs % 60) } else { format!("{}s", secs) };

        let time_color = if secs < 10 {
            theme::wait_fast()
        } else if secs < 60 {
            theme::wait_normal()
        } else if secs < 120 {
            theme::wait_slow()
        } else {
            theme::wait_very_slow()
        };

        right.push(Span::styled(
            format!("{} ", spin),
            Style::default().fg(time_color),
        ));
        if app.current_step_count > 0 {
            right.push(Span::styled(
                format!("T{}/C{}", app.current_step_count, app.current_tool_call_count),
                Style::default().fg(theme::text_secondary()),
            ));
            right.push(sep.clone());
        }
        right.push(Span::styled(
            dur,
            Style::default().fg(time_color).add_modifier(Modifier::BOLD),
        ));
        right.push(sep.clone());
        // Token speed
        if app.turn_tokens > 0 {
            let speed = app.turn_tokens as f64 / secs.max(1) as f64;
            right.push(Span::styled(
                format!("{}tok {:.0}t/s", app.turn_tokens, speed),
                Style::default().fg(theme::text_muted()),
            ));
            right.push(sep.clone());
        }
    } else if app.last_turn_duration.is_some() || app.current_step_count > 0 {
        // ── Completed turn: ✓ + duration ──
        right.push(Span::styled(
            "✓ ",
            Style::default().fg(theme::success()),
        ));
        if app.current_step_count > 0 {
            right.push(Span::styled(
                format!("T{}/C{}", app.current_step_count, app.current_tool_call_count),
                Style::default().fg(theme::text_secondary()),
            ));
            right.push(sep.clone());
        }
        if let Some(dur) = app.last_turn_duration {
            let secs = dur.as_secs();
            let dur_str = if secs >= 60 { format!("{}m{}s", secs / 60, secs % 60) } else { format!("{}s", secs) };
            right.push(Span::styled(
                dur_str,
                Style::default().fg(theme::success()),
            ));
            right.push(sep.clone());
            if app.turn_tokens > 0 {
                right.push(Span::styled(
                    format!("{}tok", app.turn_tokens),
                    Style::default().fg(theme::text_muted()),
                ));
                right.push(sep.clone());
            }
        }
    }

    // Context usage (show after first LLM call)
    if app.ctx_used_tokens > 0 && app.context_window > 0 {
        let used_k = app.ctx_used_tokens / 1000;
        let total_k = app.context_window / 1000;
        let ratio = app.ctx_used_tokens as f64 / app.context_window as f64;
        let ctx_color = if ratio < 0.5 {
            theme::success()
        } else if ratio < 0.8 {
            theme::wait_normal()
        } else {
            theme::wait_very_slow()
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
        Style::default().fg(theme::status_model()).add_modifier(Modifier::BOLD),
    ));

    // Layout
    let left_w: usize = left.iter().map(|s| display_width(&s.content)).sum();
    let right_w: usize = right.iter().map(|s| display_width(&s.content)).sum();
    let pad = width.saturating_sub(left_w + right_w);

    let mut all = left;
    all.push(Span::styled(" ".repeat(pad), Style::default()));
    all.extend(right);

    let bar = Paragraph::new(Line::from(all))
        .style(Style::default().bg(theme::bg_surface()));
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

#[allow(dead_code)]
fn truncate_session_name(name: &str, max_len: usize) -> String {
    let char_count = name.chars().count();
    if char_count <= max_len {
        name.to_string()
    } else {
        let truncated: String = name.chars().take(max_len.saturating_sub(3)).collect();
        format!("{}...", truncated)
    }
}
