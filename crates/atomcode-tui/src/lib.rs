pub mod app;
pub mod command;
pub mod event;
pub mod file_attach;
pub mod project_context;
pub mod provider_manager;
pub mod turn_log;
pub mod ui;

use std::io::Write;

use anyhow::Result;
use crossterm::{
    execute,
    event::{EnableMouseCapture, DisableMouseCapture, EnableBracketedPaste, DisableBracketedPaste},
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
        SetTitle, Clear, ClearType,
    },
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Clear the main screen and scrollback buffer before entering alternate screen.
///
/// This prevents macOS Terminal.app from scrolling "through" the alternate screen
/// to reveal the main screen's scrollback history on mouse wheel scroll-up when
/// mouse capture is temporarily disabled.
fn clear_scrollback(w: &mut impl Write) -> std::io::Result<()> {
    // \x1b[2J  — clear entire screen (so saved main screen is blank)
    // \x1b[3J  — clear scrollback buffer
    // \x1b[H   — move cursor to top-left
    w.write_all(b"\x1b[2J\x1b[3J\x1b[H")?;
    w.flush()
}

/// Flush the terminal input buffer (discard any pending input)
/// This is critical when switching between TUI and external editor
#[cfg(unix)]
fn flush_stdin() {
    unsafe {
        // TCIFLUSH = 0, flush input queue
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

#[cfg(not(unix))]
fn flush_stdin() {
    // On non-Unix systems, we can't flush stdin directly
    // Just drain any available input
    use std::io::Read;
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 64];
    while let Ok(n) = stdin.read(&mut buf) {
        if n == 0 { break; }
    }
}

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
    // Clear main screen + scrollback BEFORE entering alternate screen so Terminal.app
    // has nothing to scroll back to (prevents scrollback bleed-through on mouse wheel).
    clear_scrollback(&mut stdout)?;
    execute!(
        stdout,
        EnterAlternateScreen,
        SetTitle("AtomCode"),
        Clear(ClearType::All),
        EnableMouseCapture,
        EnableBracketedPaste,
    )?;
    // Mouse tracking + bracketed paste enabled.

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
            // First disable raw mode to release terminal control completely
            // This must happen BEFORE stopping the event loop
            disable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            )?;
            terminal.show_cursor()?;
            
            // Now stop the event loop (after raw mode is disabled)
            event_loop.stop();
            
            // Flush any pending output and DISCARD any buffered input
            let _ = std::io::stdout().flush();
            flush_stdin();

            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
            let status = std::process::Command::new(&editor)
                .arg(&file_path)
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status();

            // Show result message
            if let Ok(exit_status) = status {
                if exit_status.success() {
                    // Reload config if the edited file was the config file
                    let config_path = Config::default_path();
                    if std::path::Path::new(&file_path) == config_path {
                        match Config::load(&config_path) {
                            Ok(new_config) => {
                                app.config = new_config;
                                // Update default provider in app
                                let default_name = app.config.default_provider.clone();
                                if let Ok(provider) = app.config.active_provider(None) {
                                    app.model_name = format!("{} / {}", default_name, provider.model);
                                }
                            }
                            Err(e) => {
                                eprintln!("Failed to reload config: {}", e);
                            }
                        }
                    }
                }
            }

            // Flush stdin again before restoring TUI
            flush_stdin();

            // Restore TUI
            enable_raw_mode()?;
            clear_scrollback(terminal.backend_mut())?;
            execute!(
                terminal.backend_mut(),
                EnterAlternateScreen,
                SetTitle("AtomCode"),
                Clear(ClearType::All),
                EnableMouseCapture,
                EnableBracketedPaste,
            )?;
            terminal.clear()?;

            // Restart the event loop
            event_loop.start();
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
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
    )?;
    terminal.show_cursor()?;

    Ok(())
}
