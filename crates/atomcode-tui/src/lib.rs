pub mod app;
pub mod command;
pub mod event;
pub mod file_attach;
pub mod project_context;
pub mod provider_manager;
pub mod ui;

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

use atomcode_core::agent::AgentHandle;
use atomcode_core::config::Config;
use atomcode_core::tool::ToolContext;

use app::App;
use event::{AppEvent, EventLoop};

pub async fn run(
    config: Config,
    model_name: String,
    agent_handle: AgentHandle,
    tool_context: ToolContext,
    working_dir: std::path::PathBuf,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        SetTitle("AtomCode"),
        Clear(ClearType::All),
    )?;
    // No mouse capture at all — let terminal handle selection/copy natively.
    // Scrolling is keyboard-only (Ctrl+Up/Down, PageUp/Down).

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new(model_name, config, agent_handle, tool_context, working_dir);
    let mut event_loop = EventLoop::new();
    let event_tx = event_loop.sender();
    event_loop.start();

    loop {
        terminal.draw(|frame| ui::render(frame, &mut app))?;

        if let Some(file_path) = app.pending_editor.take() {
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
            terminal.clear()?;
            continue;
        }

        // Poll both TUI keyboard/tick events and AgentLoop events concurrently.
        // We use two separate futures so neither blocks the other.
        enum Wake {
            Tui(AppEvent),
            Agent(atomcode_core::agent::AgentEvent),
        }

        let tui_fut = event_loop.next();
        let agent_fut = app.agent_handle.event_rx.recv();

        let wake = tokio::select! {
            Some(e) = tui_fut => Wake::Tui(e),
            Some(e) = agent_fut => Wake::Agent(e),
        };

        match wake {
            Wake::Tui(event) => {
                app.handle_event(event, &event_tx);
                // Drain any remaining buffered TUI events without blocking.
                loop {
                    match event_loop.try_next() {
                        Some(e) => app.handle_event(e, &event_tx),
                        None => break,
                    }
                }
            }
            Wake::Agent(agent_event) => {
                app.handle_agent_event(agent_event);
                // Drain any additional agent events that are already queued.
                loop {
                    match app.agent_handle.event_rx.try_recv() {
                        Ok(e) => app.handle_agent_event(e),
                        Err(_) => break,
                    }
                }
            }
        }

        if app.mode.is_exiting() {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
    )?;
    terminal.show_cursor()?;

    Ok(())
}
