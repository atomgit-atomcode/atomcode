use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, AppMode};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let mode_indicator = match app.mode {
        AppMode::Normal => Span::styled(" READY ", Style::default().bg(Color::Green).fg(Color::Black)),
        AppMode::Streaming => Span::styled(" STREAMING ", Style::default().bg(Color::Yellow).fg(Color::Black)),
        AppMode::ProviderManager => Span::styled(" PROVIDERS ", Style::default().bg(Color::Magenta).fg(Color::White)),
        AppMode::Exiting => Span::styled(" EXITING ", Style::default().bg(Color::Red).fg(Color::White)),
    };

    let model = Span::styled(
        format!("  model: {} ", app.provider.model_name()),
        Style::default().fg(Color::Gray),
    );

    let title = Span::styled(
        " AtomCode v0.1.0 ",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    let line = Line::from(vec![title, mode_indicator, model]);
    let bar = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(bar, area);
}
