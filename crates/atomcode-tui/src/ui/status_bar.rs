use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, AppMode};

const BAR_BG: Color = Color::Rgb(20, 20, 25);
const DIM: Color = Color::Rgb(70, 70, 70);
const ACCENT: Color = Color::Rgb(130, 100, 255);

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;

    // Left side: logo + working dir
    let dir = shorten_path(&app.working_dir.to_string_lossy());

    // Right side: model + mode
    let model_name = app.provider.model_name();
    let mode_str = match &app.mode {
        AppMode::Normal => "",
        AppMode::Streaming => " \u{25cf} streaming",        // ●
        AppMode::WaitingApproval(_) => " \u{25cf} approval",
        AppMode::ToolExecuting => " \u{25cf} executing",
        AppMode::ProviderManager => " \u{25cf} providers",
        AppMode::Exiting => " \u{25cf} exiting",
    };
    let mode_color = match &app.mode {
        AppMode::Normal => DIM,
        AppMode::Streaming => Color::Rgb(240, 200, 60),
        AppMode::WaitingApproval(_) => Color::Rgb(240, 200, 60),
        AppMode::ToolExecuting => Color::Rgb(80, 200, 255),
        AppMode::ProviderManager => Color::Rgb(180, 140, 255),
        AppMode::Exiting => Color::Rgb(240, 80, 80),
    };

    let right_text = format!("{}{}", model_name, mode_str);
    let left_text = format!(" \u{2666} {} ", dir); // ♦ ~/project/foo

    // Calculate padding
    let pad = width.saturating_sub(left_text.len() + right_text.len() + 1);

    let line = Line::from(vec![
        Span::styled(
            " \u{2666} ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} ", dir),
            Style::default().fg(Color::Rgb(140, 140, 140)),
        ),
        Span::styled(" ".repeat(pad), Style::default()),
        Span::styled(
            model_name.to_string(),
            Style::default().fg(DIM),
        ),
        Span::styled(
            mode_str.to_string(),
            Style::default().fg(mode_color),
        ),
        Span::raw(" "),
    ]);

    let bar = Paragraph::new(line).style(Style::default().bg(BAR_BG));
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
