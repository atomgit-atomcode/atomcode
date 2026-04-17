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
    "quit", "help", "config", "clear", "model", "status", "diff", "undo", "cost", "cd",
];

pub async fn run(
    config: Config,
    model_name: String,
    mut agent_handle: AgentHandle,
    _tool_context: ToolContext,
    mut working_dir: std::path::PathBuf,
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

    // Welcome screen
    let wd_str = working_dir.to_string_lossy().to_string();
    let dir_display = if let Some(home) = std::env::var("HOME").ok() {
        wd_str.replacen(&home, "~", 1)
    } else {
        wd_str
    };
    render::print_welcome(&model_name, &dir_display);

    // Set up scroll region: output area above, fixed input box below
    render::setup_scroll_region();
    render::draw_input_box("", 0);

    let mut history: Vec<String> = Vec::new();
    let mut streaming = false;
    let mut spinner_frame: usize = 0;
    let mut spinner_label = String::from("Thinking...");
    let mut total_tokens: usize = 0;
    let mut previous_dir: Option<std::path::PathBuf> = None;
    let mut streaming_started = false; // track if we printed the initial bar prefix
    let mut think_buf = String::new(); // accumulator for in-flight <think> stripping

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
                                     \x20   /cost    - Show token cost\r\n\
                                     \x20   /cd      - Change directory\r\n"
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
                            "/status" => {
                                let mut out = io::stdout();
                                let _ = write!(
                                    out,
                                    "\r\n  Model:  {}\r\n  Dir:    {}\r\n  Config: {}\r\n",
                                    model_name,
                                    working_dir.display(),
                                    Config::default_path().display(),
                                );
                                let _ = out.flush();
                                continue;
                            }
                            "/diff" => {
                                let output = std::process::Command::new("git")
                                    .args(["diff", "--stat"])
                                    .current_dir(&working_dir)
                                    .output();
                                let mut out = io::stdout();
                                match output {
                                    Ok(o) => {
                                        let text = String::from_utf8_lossy(&o.stdout);
                                        for line in text.lines() {
                                            let _ = write!(out, "\r\n  {}", line);
                                        }
                                        let _ = write!(out, "\r\n");
                                    }
                                    Err(e) => {
                                        let _ = write!(out, "\r\n  git diff failed: {}\r\n", e);
                                    }
                                }
                                let _ = out.flush();
                                continue;
                            }
                            "/undo" => {
                                let mut out = io::stdout();
                                let _ = write!(out, "\r\n  Undo is not yet supported.\r\n");
                                let _ = out.flush();
                                continue;
                            }
                            "/cost" => {
                                let mut out = io::stdout();
                                let _ = write!(out, "\r\n  Session tokens: {}\r\n", total_tokens);
                                let _ = out.flush();
                                continue;
                            }
                            "/cd" => {
                                let arg = text.strip_prefix("/cd").unwrap().trim();
                                let home_dir = std::env::var("HOME").ok().map(std::path::PathBuf::from);
                                let mut out = io::stdout();
                                if arg.is_empty() {
                                    // /cd with no arg: go home
                                    if let Some(home) = &home_dir {
                                        previous_dir = Some(working_dir.clone());
                                        working_dir = home.clone();
                                        agent_handle.cmd_tx.send(AgentCommand::ChangeDir(working_dir.to_string_lossy().to_string())).ok();
                                        let _ = write!(out, "\r\n  Changed to: {}\r\n", working_dir.display());
                                    }
                                } else {
                                    let path = if arg == "-" {
                                        previous_dir.clone().unwrap_or_else(|| working_dir.clone())
                                    } else if arg.starts_with('~') {
                                        if let Some(home) = &home_dir {
                                            let rest = arg.strip_prefix("~/").unwrap_or(&arg[1..]);
                                            if rest.is_empty() { home.clone() } else { home.join(rest) }
                                        } else {
                                            std::path::PathBuf::from(arg)
                                        }
                                    } else {
                                        let p = std::path::PathBuf::from(arg);
                                        if p.is_absolute() { p } else { working_dir.join(p) }
                                    };
                                    if path.is_dir() {
                                        previous_dir = Some(working_dir.clone());
                                        working_dir = path;
                                        agent_handle.cmd_tx.send(AgentCommand::ChangeDir(working_dir.to_string_lossy().to_string())).ok();
                                        let _ = write!(out, "\r\n  Changed to: {}\r\n", working_dir.display());
                                    } else {
                                        let _ = write!(out, "\r\n  Not a directory: {}\r\n", path.display());
                                    }
                                }
                                let _ = out.flush();
                                continue;
                            }
                            _ => {
                                // Unknown slash command — send to agent as regular message
                            }
                        }
                    }

                    history.push(text.clone());
                    render::move_to_scroll_end();
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
                            // Strip <think>...</think> blocks in-flight.
                            let visible = strip_think_streaming(&text, &mut think_buf);
                            if visible.is_empty() { continue; }

                            render::move_to_scroll_end();
                            let mut out = io::stdout();
                            // First visible delta: print bar prefix
                            if !streaming_started {
                                let _ = write!(out, "\r\n  \u{2502} ");
                                streaming_started = true;
                            }
                            // Replace \n with \r\n + bar prefix for each new line
                            let fixed = visible.replace('\n', "\r\n  \u{2502} ");
                            let _ = write!(out, "{}", fixed);
                            let _ = out.flush();
                            // Redraw input box (streaming may have scrolled content)
                            render::draw_input_box("", 0);
                        }
                        Some(AgentEvent::ToolCallStreaming { name, .. }) => {
                            spinner_label = format!("Preparing {}...", name);
                        }
                        Some(AgentEvent::ToolCallStarted { name, arguments, .. }) => {
                            render::clear_spinner();
                            let mut out = io::stdout();
                            // End streaming text line if active
                            if streaming_started {
                                let _ = write!(out, "\r\n");
                            }
                            let _ = out.flush();
                            let detail = render::format_tool_detail(&name, &arguments);
                            render::print_tool_call(&name, &detail);
                            spinner_label = format!("Running {}...", name);
                            streaming_started = false; // next TextDelta gets fresh bar prefix
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
                            streaming_started = false;
                            think_buf.clear();
                        }
                        Some(AgentEvent::TurnCancelled { .. }) => {
                            render::clear_spinner();
                            let mut out = io::stdout();
                            let _ = write!(out, "\r\n  (cancelled)\r\n");
                            let _ = out.flush();
                            streaming = false;
                            streaming_started = false;
                        }
                        Some(AgentEvent::Error(e)) => {
                            render::clear_spinner();
                            render::print_error(&e);
                            streaming = false;
                            streaming_started = false;
                        }
                        Some(AgentEvent::TokenUsage(usage)) => {
                            total_tokens += usage.completion_tokens;
                        }
                        Some(AgentEvent::ContextStats { .. }
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
    render::reset_scroll_region();
    if !cfg!(target_os = "windows") {
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    }
    terminal::disable_raw_mode()?;
    let _ = write!(io::stdout(), "\r\n");
    let _ = io::stdout().flush();

    Ok(())
}

/// Strip `<think>...</think>` blocks from streaming text.
/// `buf` accumulates partial tags across delta boundaries.
/// Returns the visible text (everything outside think blocks).
fn strip_think_streaming(delta: &str, buf: &mut String) -> String {
    buf.push_str(delta);
    let mut visible = String::new();
    let mut rest = buf.as_str();

    loop {
        match rest.find("<think>") {
            None => {
                // Check if rest ends with a partial "<thi..." that might be start of tag
                let maybe_partial = rest.len().min(7);
                let tail = &rest[rest.len() - maybe_partial..];
                if "<think>".starts_with(tail) && !tail.is_empty() && tail != rest {
                    // Might be partial tag — hold it in buffer
                    visible.push_str(&rest[..rest.len() - maybe_partial]);
                    *buf = tail.to_string();
                } else {
                    visible.push_str(rest);
                    buf.clear();
                }
                break;
            }
            Some(open) => {
                visible.push_str(&rest[..open]);
                let after = &rest[open + 7..]; // skip "<think>"
                match after.find("</think>") {
                    Some(close) => {
                        // Complete block — skip it, continue scanning
                        rest = &after[close + 8..]; // skip "</think>"
                    }
                    None => {
                        // Block still open — swallow everything, hold in buffer
                        *buf = rest[open..].to_string();
                        break;
                    }
                }
            }
        }
    }
    visible
}
