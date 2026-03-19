use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, AppMode};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let mode_indicator = match &app.mode {
        AppMode::Normal => Span::styled(" READY ", Style::default().bg(Color::Green).fg(Color::Black)),
        AppMode::Streaming => Span::styled(" STREAMING ", Style::default().bg(Color::Yellow).fg(Color::Black)),
        AppMode::WaitingApproval(_) => Span::styled(" APPROVAL ", Style::default().bg(Color::Yellow).fg(Color::Black)),
        AppMode::ToolExecuting => Span::styled(" EXECUTING ", Style::default().bg(Color::Cyan).fg(Color::Black)),
        AppMode::ProviderManager => Span::styled(" PROVIDERS ", Style::default().bg(Color::Magenta).fg(Color::White)),
        AppMode::Exiting => Span::styled(" EXITING ", Style::default().bg(Color::Red).fg(Color::White)),
    };

    let model = Span::styled(
        format!("  {} ", app.provider.model_name()),
        Style::default().fg(Color::Gray),
    );

    // Show shortened working directory
    let dir_display = shorten_path(&app.working_dir.to_string_lossy());
    let dir = Span::styled(
        format!("  {} ", dir_display),
        Style::default().fg(Color::Rgb(100, 100, 100)),
    );

    let title = Span::styled(
        " AtomCode ",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    let line = Line::from(vec![title, mode_indicator, model, dir]);
    let bar = Paragraph::new(line).style(Style::default().bg(Color::Rgb(25, 25, 30)));
    frame.render_widget(bar, area);
}

/// Shorten home directory to ~ and keep last 2 components if path is long.
fn shorten_path(path: &str) -> String {
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    let shortened = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    if shortened.len() > 40 {
        let parts: Vec<&str> = shortened.rsplitn(3, '/').collect();
        if parts.len() >= 3 {
            format!(".../{}/ {}", parts[1], parts[0])
        } else {
            shortened
        }
    } else {
        shortened
    }
}
