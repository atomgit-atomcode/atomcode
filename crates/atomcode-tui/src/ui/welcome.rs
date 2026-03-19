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

    // Top padding
    let logo_height = 7; // logo lines + spacing
    let info_height = 8;
    let total_content = logo_height + info_height;
    let top_pad = if area.height as usize > total_content {
        (area.height as usize - total_content) / 3
    } else {
        1
    };

    for _ in 0..top_pad {
        lines.push(Line::default());
    }

    // Logo
    let logo_color = Color::Rgb(130, 100, 255); // Purple-ish
    for logo_line in LOGO.lines().skip(1) {
        lines.push(Line::from(Span::styled(
            logo_line.to_string(),
            Style::default().fg(logo_color).add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::default());

    // Version + model + working dir
    lines.push(Line::from(vec![
        Span::styled(
            "  v0.1.0",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("  ·  model: {}", app.provider.model_name()),
            Style::default().fg(Color::Cyan),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            format!("  cwd: {}", app.working_dir.display()),
            Style::default().fg(Color::Rgb(80, 80, 80)),
        ),
    ]));

    lines.push(Line::default());

    // Tips
    let dim = Style::default().fg(Color::DarkGray);
    let key_style = Style::default().fg(Color::Gray);

    lines.push(Line::from(vec![
        Span::styled("  Tips: ", Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    Enter      ", key_style),
        Span::styled("Send message", dim),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    /          ", key_style),
        Span::styled("Show commands", dim),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    /provider  ", key_style),
        Span::styled("Manage providers", dim),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    Esc        ", key_style),
        Span::styled("Clear input", dim),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    /quit      ", key_style),
        Span::styled("Exit AtomCode", dim),
    ]));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}
