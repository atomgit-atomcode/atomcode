pub mod app;
pub mod event;
pub mod ui;

use anyhow::Result;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use atomcode_core::config::Config;
use atomcode_core::provider::LlmProvider;

use app::{App, AppMode};
use event::EventLoop;

pub async fn run(config: Config, provider: Box<dyn LlmProvider>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(provider, config);
    let mut event_loop = EventLoop::new();
    let event_tx = event_loop.sender();
    event_loop.start();

    loop {
        terminal.draw(|frame| ui::render(frame, &app))?;

        // Handle pending editor (e.g., /config)
        if let Some(file_path) = app.pending_editor.take() {
            // Temporarily leave TUI
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
            terminal.show_cursor()?;

            // Open editor
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
            let _ = std::process::Command::new(&editor)
                .arg(&file_path)
                .status();

            // Re-enter TUI
            enable_raw_mode()?;
            execute!(terminal.backend_mut(), EnterAlternateScreen)?;
            terminal.clear()?;
            continue;
        }

        if let Some(event) = event_loop.next().await {
            app.handle_event(event, &event_tx);
        }

        if app.mode == AppMode::Exiting {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
