use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, WelcomeState};

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

    let logo_color = Color::Rgb(120, 95, 235);
    for logo_line in LOGO.lines().skip(1) {
        lines.push(Line::from(Span::styled(
            logo_line.to_string(),
            Style::default().fg(logo_color).add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::default());

    // Version + model
    lines.push(Line::from(vec![
        Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), Style::default().fg(Color::Rgb(65, 68, 80))),
        Span::styled(
            format!("  \u{00b7}  {}", app.model_name),
            Style::default().fg(Color::Rgb(100, 170, 235)),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!("  {}", app.working_dir.display()),
        Style::default().fg(Color::Rgb(60, 62, 72)),
    )));

    lines.push(Line::default());

    let h = Style::default().fg(Color::Rgb(130, 132, 145)).add_modifier(Modifier::BOLD);
    let k = Style::default().fg(Color::Rgb(105, 150, 210));
    let d = Style::default().fg(Color::Rgb(85, 88, 100));

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

/// Full-screen first-run setup screen shown when no providers are configured.
pub fn render_setup(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let logo_height = 7;
    let content_height = logo_height + 14;
    let top_pad = if area.height as usize > content_height {
        (area.height as usize - content_height) / 3
    } else {
        1
    };

    for _ in 0..top_pad {
        lines.push(Line::default());
    }

    // Logo
    let logo_color = Color::Rgb(120, 95, 235);
    for logo_line in LOGO.lines().skip(1) {
        lines.push(Line::from(Span::styled(
            logo_line.to_string(),
            Style::default().fg(logo_color).add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Welcome! Choose how to get started:",
        Style::default().fg(Color::Rgb(180, 182, 195)),
    )));
    lines.push(Line::default());

    let options = [
        "Login with AtomGit",
        "Configure manually",
        "Skip for now",
    ];
    for (i, opt) in options.iter().enumerate() {
        let is_sel = i == state.selected;
        let prefix = if is_sel { "  \u{25b6} " } else { "     " };
        let (fg, bg) = if is_sel {
            (Color::Rgb(240, 240, 245), Color::Rgb(50, 40, 100))
        } else {
            (Color::Rgb(140, 142, 155), Color::Reset)
        };
        let number = format!("[{}] ", i + 1);
        lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(fg).bg(bg)),
            Span::styled(number, Style::default().fg(Color::Rgb(105, 150, 210)).bg(bg)),
            Span::styled(opt.to_string(), Style::default().fg(fg).bg(bg)),
        ]));
    }

    lines.push(Line::default());

    // Error message if any
    if let Some(ref err) = state.error {
        lines.push(Line::from(Span::styled(
            format!("  Error: {}", err),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::default());
    }

    lines.push(Line::from(Span::styled(
        "  \u{2191}\u{2193} / 1-3 to select   Enter to confirm",
        Style::default().fg(Color::Rgb(65, 68, 80)),
    )));

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
