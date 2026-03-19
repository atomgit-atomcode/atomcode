pub mod app;
pub mod command;
pub mod event;
pub mod file_attach;
pub mod provider_manager;
pub mod ui;

use std::io::Write as _;

use anyhow::Result;
use crossterm::{
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
        SetTitle, Clear, ClearType,
    },
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use atomcode_core::config::Config;
use atomcode_core::provider::LlmProvider;
use atomcode_core::tool::ToolRegistry;

use app::App;
use event::EventLoop;

pub async fn run(config: Config, provider: Box<dyn LlmProvider>, tool_registry: ToolRegistry, working_dir: std::path::PathBuf) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        SetTitle("AtomCode"),
        Clear(ClearType::All),
    )?;
    // Enable mouse button tracking (scroll wheel) but NOT drag tracking
    // \x1b[?1000h = normal tracking (button press/release only, includes scroll)
    // \x1b[?1006h = SGR extended mode (for large terminals)
    // This does NOT capture drag/selection — terminal handles that natively
    write!(stdout, "\x1b[?1000h\x1b[?1006h")?;
    stdout.flush()?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new(provider, config, tool_registry, working_dir);
    let mut event_loop = EventLoop::new();
    let event_tx = event_loop.sender();
    event_loop.start();

    loop {
        terminal.draw(|frame| ui::render(frame, &mut app))?;

        if let Some(file_path) = app.pending_editor.take() {
            // Disable mouse tracking before leaving
            write!(terminal.backend_mut(), "\x1b[?1000l\x1b[?1006l")?;
            terminal.backend_mut().flush()?;
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
            terminal.show_cursor()?;

            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
            let _ = std::process::Command::new(&editor)
                .arg(&file_path)
                .status();

            enable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                EnterAlternateScreen,
                Clear(ClearType::All),
            )?;
            write!(terminal.backend_mut(), "\x1b[?1000h\x1b[?1006h")?;
            terminal.backend_mut().flush()?;
            terminal.clear()?;
            continue;
        }

        if let Some(event) = event_loop.next().await {
            app.handle_event(event, &event_tx);
            loop {
                match event_loop.try_next() {
                    Some(event) => app.handle_event(event, &event_tx),
                    None => break,
                }
            }
        }

        if app.mode.is_exiting() {
            break;
        }
    }

    write!(terminal.backend_mut(), "\x1b[?1000l\x1b[?1006l")?;
    terminal.backend_mut().flush()?;
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
    )?;
    terminal.show_cursor()?;

    Ok(())
}
