pub mod input;
pub mod render;

use anyhow::Result;
use atomcode_core::agent::{AgentCommand, AgentEvent, AgentHandle, AgentPhase};
use atomcode_core::config::Config;
use atomcode_core::tool::ToolContext;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyModifiers,
};
use crossterm::terminal;
use std::io::{self, Write};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Slash commands available for tab completion (without leading `/`).
const COMMANDS: &[&str] = &[
    "quit", "help", "config", "clear", "model", "status", "diff", "undo", "cost",
];

pub async fn run(
    config: Config,
    model_name: String,
    mut agent_handle: AgentHandle,
    _tool_context: ToolContext,
    working_dir: std::path::PathBuf,
    _session_to_continue: Option<atomcode_core::session::Session>,
) -> Result<()> {
    // Enable raw mode (no alternate screen)
    terminal::enable_raw_mode()?;

    // Enable bracketed paste on non-Windows
    if !cfg!(target_os = "windows") {
        let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
    }

    // Build commands list for tab completion
    let commands: Vec<String> = COMMANDS.iter().map(|s| s.to_string()).collect();

    // Print header
    let wd_str = working_dir.to_string_lossy().to_string();
    let dir_display = if let Some(home) = std::env::var("HOME").ok() {
        wd_str.replacen(&home, "~", 1)
    } else {
        wd_str
    };
    render::print_header(&model_name, &dir_display);

    let mut history: Vec<String> = Vec::new();
    let mut streaming = false;
    let mut spinner_frame: usize = 0;
    let mut spinner_label = String::from("Thinking...");

    loop {
        if !streaming {
            // Input mode: block until user submits
            match input::read_input(&history, &commands) {
                Some(text) => {
                    // Handle slash commands
                    if text.starts_with('/') {
                        let cmd = text.split_whitespace().next().unwrap_or("");
                        match cmd {
                            "/quit" | "/exit" => break,
                            "/help" => {
                                let mut out = io::stdout();
                                let _ = write!(
                                    out,
                                    "\r\n  Available commands:\r\n\
                                     \x20   /quit    - Exit AtomCode\r\n\
                                     \x20   /help    - Show this help\r\n\
                                     \x20   /config  - Show config path\r\n\
                                     \x20   /clear   - Clear screen\r\n\
                                     \x20   /model   - Show current model\r\n\
                                     \x20   /status  - Show session status\r\n\
                                     \x20   /diff    - Show git diff\r\n\
                                     \x20   /undo    - Undo last change\r\n\
                                     \x20   /cost    - Show token cost\r\n"
                                );
                                let _ = out.flush();
                                continue;
                            }
                            "/config" => {
                                let mut out = io::stdout();
                                let _ = write!(out, "\r\n  Provider: {}\r\n", config.default_provider);
                                let _ = out.flush();
                                continue;
                            }
                            "/clear" => {
                                let mut out = io::stdout();
                                let _ = crossterm::execute!(
                                    out,
                                    crossterm::terminal::Clear(
                                        crossterm::terminal::ClearType::All
                                    ),
                                    crossterm::cursor::MoveTo(0, 0)
                                );
                                render::print_header(&model_name, &dir_display);
                                continue;
                            }
                            "/model" => {
                                let mut out = io::stdout();
                                let _ = write!(out, "\r\n  Model: {}\r\n", model_name);
                                let _ = out.flush();
                                continue;
                            }
                            _ => {
                                // Unknown slash command — send to agent as regular message
                            }
                        }
                    }

                    history.push(text.clone());
                    render::print_user_message(&text);
                    agent_handle
                        .cmd_tx
                        .send(AgentCommand::SendMessage(text))
                        .ok();
                    streaming = true;
                    spinner_label = "Thinking...".to_string();
                    spinner_frame = 0;
                }
                None => {
                    // Ctrl+C on empty buffer — exit
                    break;
                }
            }
        } else {
            // Streaming mode: select on agent events, spinner ticks, and key events
            tokio::select! {
                event = agent_handle.event_rx.recv() => {
                    match event {
                        Some(AgentEvent::TextDelta(text)) => {
                            render::clear_spinner();
                            // Print the streaming text chunk directly
                            let mut out = io::stdout();
                            let _ = write!(out, "{}", text);
                            let _ = out.flush();
                        }
                        Some(AgentEvent::ToolCallStreaming { name, .. }) => {
                            spinner_label = format!("Preparing {}...", name);
                        }
                        Some(AgentEvent::ToolCallStarted { name, arguments, .. }) => {
                            render::clear_spinner();
                            // End any streaming text line
                            let mut out = io::stdout();
                            let _ = write!(out, "\r\n");
                            let _ = out.flush();
                            let detail = render::format_tool_detail(&name, &arguments);
                            render::print_tool_call(&name, &detail);
                            spinner_label = format!("Running {}...", name);
                        }
                        Some(AgentEvent::ToolCallResult { output, success, name, .. }) => {
                            render::clear_spinner();
                            // Summarize: first line or truncated
                            let summary = output.lines().next().unwrap_or("(no output)");
                            let summary = if summary.len() > 80 {
                                format!("{}...", &summary[..77])
                            } else {
                                summary.to_string()
                            };
                            render::print_tool_result(success, &summary);

                            // Print diff lines if present
                            for line in output.lines() {
                                if line.starts_with("- ") || line.starts_with("+ ") {
                                    render::print_diff_line(line);
                                }
                            }
                            let _ = name; // suppress unused warning
                        }
                        Some(AgentEvent::PhaseChange(AgentPhase::Thinking)) => {
                            spinner_label = "Thinking...".to_string();
                        }
                        Some(AgentEvent::PhaseChange(AgentPhase::CallingTool(name))) => {
                            spinner_label = format!("Preparing {}...", name);
                        }
                        Some(AgentEvent::PhaseChange(_)) => {}
                        Some(AgentEvent::ApprovalNeeded { tool_name, call, .. }) => {
                            render::clear_spinner();
                            let detail = render::format_tool_detail(&tool_name, &call.arguments);
                            render::print_approval_prompt(&tool_name, &detail);
                            let choice = input::read_approval();
                            match choice {
                                'y' => {
                                    agent_handle.cmd_tx.send(AgentCommand::ApproveTool).ok();
                                }
                                'a' => {
                                    agent_handle.cmd_tx.send(AgentCommand::ApproveToolAlways).ok();
                                }
                                _ => {
                                    agent_handle.cmd_tx.send(AgentCommand::DenyTool).ok();
                                }
                            }
                        }
                        Some(AgentEvent::TurnComplete { .. }) => {
                            render::clear_spinner();
                            let mut out = io::stdout();
                            let _ = write!(out, "\r\n");
                            let _ = out.flush();
                            streaming = false;
                        }
                        Some(AgentEvent::TurnCancelled { .. }) => {
                            render::clear_spinner();
                            let mut out = io::stdout();
                            let _ = write!(out, "\r\n  (cancelled)\r\n");
                            let _ = out.flush();
                            streaming = false;
                        }
                        Some(AgentEvent::Error(e)) => {
                            render::clear_spinner();
                            render::print_error(&e);
                            streaming = false;
                        }
                        Some(AgentEvent::TokenUsage(_)
                            | AgentEvent::ContextStats { .. }
                            | AgentEvent::SubAgentProgress { .. }
                            | AgentEvent::WorkingDirChanged(_)) => {
                            // Ignored for now
                        }
                        None => {
                            // Agent channel closed
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    // Spinner tick
                    spinner_frame = (spinner_frame + 1) % SPINNER_FRAMES.len();
                    render::print_spinner(SPINNER_FRAMES[spinner_frame], &spinner_label);

                    // Also poll for Ctrl+C during streaming
                    if event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
                        if let Ok(Event::Key(key)) = event::read() {
                            if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                agent_handle.cmd_tx.send(AgentCommand::Cancel).ok();
                            }
                        }
                    }
                }
            }
        }
    }

    // Cleanup
    if !cfg!(target_os = "windows") {
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    }
    terminal::disable_raw_mode()?;
    println!();

    Ok(())
}
