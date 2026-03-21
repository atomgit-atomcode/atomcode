use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;

const LOGO: &str = r#"
     _   _                  ____          _
    / \ | |_ ___  _ __ ___ / ___|___   __| | ___
   / _ \| __/ _ \| '_ ` _ \ |   / _ \ / _` |/ _ \
  / ___ \ || (_) | | | | | | |__| (_) | (_| |  __/
 /_/   \_\__\___/|_| |_| |_|\____\___/ \__,_|\___|
"#;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let logo_height = 7;
    let info_height = 18;
    let total_content = logo_height + info_height;
    let top_pad = if area.height as usize > total_content {
        (area.height as usize - total_content) / 3
    } else {
        1
    };

    for _ in 0..top_pad {
        lines.push(Line::default());
    }

    let logo_color = Color::Rgb(130, 100, 255);
    for logo_line in LOGO.lines().skip(1) {
        lines.push(Line::from(Span::styled(
            logo_line.to_string(),
            Style::default().fg(logo_color).add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::default());

    // Version + model
    lines.push(Line::from(vec![
        Span::styled("  v0.9.0", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("  ·  model: {}", app.model_name),
            Style::default().fg(Color::Cyan),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!("  cwd: {}", app.working_dir.display()),
        Style::default().fg(Color::Rgb(80, 80, 80)),
    )));

    lines.push(Line::default());

    let h = Style::default().fg(Color::Rgb(140, 140, 150)).add_modifier(Modifier::BOLD);
    let k = Style::default().fg(Color::Rgb(120, 160, 220));
    let d = Style::default().fg(Color::Rgb(100, 100, 110));

    // Input
    lines.push(Line::from(Span::styled("  Input", h)));
    lines.push(line_kv("    Enter", "Send message", k, d));
    lines.push(line_kv("    Shift+Enter", "New line", k, d));
    lines.push(line_kv("    Esc", "Clear input", k, d));
    lines.push(line_kv("    Up/Down", "Browse history", k, d));
    lines.push(line_kv("    Tab", "Accept suggestion", k, d));

    lines.push(Line::default());

    // Navigation
    lines.push(Line::from(Span::styled("  Navigation", h)));
    lines.push(line_kv("    Ctrl+Up/Down", "Scroll (3 lines)", k, d));
    lines.push(line_kv("    PageUp/Down", "Scroll (page)", k, d));
    lines.push(line_kv("    Ctrl+L", "Clear conversation", k, d));

    lines.push(Line::default());

    // Editing
    lines.push(Line::from(Span::styled("  Editing", h)));
    lines.push(line_kv("    Ctrl+A / Home", "Line start", k, d));
    lines.push(line_kv("    Ctrl+E / End", "Line end", k, d));
    lines.push(line_kv("    Ctrl+U", "Clear line", k, d));
    lines.push(line_kv("    Ctrl+K", "Delete to end", k, d));
    lines.push(line_kv("    Ctrl+W", "Delete word", k, d));

    lines.push(Line::default());

    // Commands
    lines.push(Line::from(Span::styled("  Commands", h)));
    lines.push(line_kv("    /", "Show all commands", k, d));
    lines.push(line_kv("    /model", "Switch model", k, d));
    lines.push(line_kv("    /provider", "Manage providers", k, d));
    lines.push(line_kv("    Ctrl+C", "Cancel / double to exit", k, d));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

fn line_kv(key: &str, desc: &str, ks: Style, ds: Style) -> Line<'static> {
    let pad = 20usize.saturating_sub(key.len());
    Line::from(vec![
        Span::styled(key.to_string(), ks),
        Span::styled(" ".repeat(pad), ds),
        Span::styled(desc.to_string(), ds),
    ])
}
