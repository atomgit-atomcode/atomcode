use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, AppMode};

/// VS Code style bottom-bar aesthetic: dark bg, subtle separators, clean typography
const BG: Color = Color::Rgb(30, 30, 35);
const FG: Color = Color::Rgb(160, 160, 170);
const DIM: Color = Color::Rgb(80, 80, 90);
const SEP: &str = " \u{2502} "; // │ thin separator

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let dir = shorten_path(&app.working_dir.to_string_lossy());
    let model = app.provider.model_name();

    // Mode: icon + text
    let (mode_icon, mode_text, mode_fg) = match &app.mode {
        AppMode::Normal => ("\u{2713}", "", Color::Rgb(80, 180, 100)),  // ✓
        AppMode::Streaming => ("\u{25b6}", "streaming", Color::Rgb(240, 180, 40)),  // ▶
        AppMode::WaitingApproval(_) => ("\u{25cf}", "approval", Color::Rgb(240, 180, 40)),  // ●
        AppMode::ToolExecuting => ("\u{25b6}", "running", Color::Rgb(80, 180, 240)),  // ▶
        AppMode::ProviderManager => ("\u{2699}", "config", Color::Rgb(180, 140, 255)),  // ⚙
        AppMode::Exiting => ("\u{00d7}", "exit", Color::Rgb(240, 80, 80)),  // ×
    };

    // Build left segments
    let mut spans: Vec<Span> = Vec::new();

    // App name
    spans.push(Span::styled(
        " atomcode ",
        Style::default().fg(Color::White).bg(Color::Rgb(90, 60, 180)).add_modifier(Modifier::BOLD),
    ));

    // Mode indicator
    spans.push(Span::styled(
        format!(" {} ", mode_icon),
        Style::default().fg(mode_fg),
    ));
    if !mode_text.is_empty() {
        spans.push(Span::styled(
            mode_text.to_string(),
            Style::default().fg(mode_fg),
        ));
    }

    // Separator + directory
    spans.push(Span::styled(SEP, Style::default().fg(DIM)));
    spans.push(Span::styled(
        dir,
        Style::default().fg(FG),
    ));

    // Elapsed time (cumulative for current turn)
    let (time_str, time_color) = if let Some(start) = app.turn_start {
        let secs = start.elapsed().as_secs();
        (format_duration(secs), Color::Rgb(240, 180, 40)) // yellow while active
    } else if let Some(dur) = app.last_turn_duration {
        (format_duration(dur.as_secs()), Color::Rgb(80, 180, 100)) // green when done
    } else {
        (String::new(), DIM)
    };

    if !time_str.is_empty() {
        spans.push(Span::styled(SEP, Style::default().fg(DIM)));
        spans.push(Span::styled(
            time_str,
            Style::default().fg(time_color),
        ));
    }

    // Token counter
    let token_str = if app.total_tokens > 0 {
        if app.turn_tokens > 0 && app.turn_start.is_some() {
            format!("~{}t (+{})", format_tokens(app.total_tokens), format_tokens(app.turn_tokens))
        } else {
            format!("~{}t", format_tokens(app.total_tokens))
        }
    } else {
        String::new()
    };

    if !token_str.is_empty() {
        spans.push(Span::styled(SEP, Style::default().fg(DIM)));
        spans.push(Span::styled(
            token_str,
            Style::default().fg(DIM),
        ));
    }

    // Right side: model name (right-aligned)
    let right = format!(" {} ", model);
    let left_len: usize = spans.iter().map(|s| s.content.len()).sum();
    let pad = width.saturating_sub(left_len + right.len());

    spans.push(Span::styled(" ".repeat(pad), Style::default()));
    spans.push(Span::styled(
        right,
        Style::default().fg(DIM),
    ));

    let bar = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(BG));
    frame.render_widget(bar, area);
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

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!(" {}s ", secs)
    } else {
        format!(" {}m{}s ", secs / 60, secs % 60)
    }
}

fn format_tokens(n: usize) -> String {
    if n < 1000 {
        format!("{}", n)
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}
