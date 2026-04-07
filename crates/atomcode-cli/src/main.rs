use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use atomcode_core::agent::{AgentCommand, AgentEvent, AgentLoop};
use atomcode_core::config::provider::{ProviderConfig, default_context_window_for};
use atomcode_core::config::Config;
use atomcode_core::conversation::Conversation;
use atomcode_core::provider::create_provider;
use atomcode_core::session::SessionManager;
use atomcode_core::tool::{ToolContext, ToolRegistry};
use atomcode_core::tool::read::ReadFileTool;
use atomcode_core::tool::write::WriteFileTool;
use atomcode_core::tool::edit::EditFileTool;
use atomcode_core::tool::bash::BashTool;
use atomcode_core::tool::grep::GrepTool;
use atomcode_core::tool::glob::GlobTool;
use atomcode_core::tool::list_dir::ListDirTool;
use atomcode_core::tool::web_search::WebSearchTool;
use atomcode_core::tool::web_fetch::WebFetchTool;
use atomcode_core::tool::search_replace::SearchReplaceTool;

mod auth;

#[derive(Parser)]
#[command(name = "atomcode", version = env!("CARGO_PKG_VERSION"), about = "AI coding assistant in your terminal")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Continue the previous session instead of starting a new one
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,

    /// Provider to use (overrides config default)
    #[arg(long)]
    provider: Option<String>,

    /// Model to use (overrides config provider model)
    #[arg(long)]
    model: Option<String>,

    /// Path to config file
    #[arg(long)]
    config: Option<PathBuf>,

    /// Working directory (defaults to current directory)
    #[arg(long, short = 'C')]
    dir: Option<PathBuf>,

    /// Run in headless mode (no TUI, just execute the prompt)
    #[arg(long)]
    headless: bool,

    /// Prompt to send in headless mode (required if --headless)
    #[arg(short = 'p', long)]
    prompt: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Login to AtomCode using AtomGit OAuth
    Login,
    /// Logout from AtomCode
    Logout,
    /// Show current login status
    Status,
}

#[tokio::main]
async fn main() {
    // Set panic hook to show errors cleanly
    std::panic::set_hook(Box::new(|info| {
        // Restore terminal before printing panic
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
        );
        eprintln!("\nAtomCode crashed: {}", info);
        if let Some(location) = info.location() {
            eprintln!("  at {}:{}:{}", location.file(), location.line(), location.column());
        }
        eprintln!("\nPlease report this at: https://github.com/atomcode/atomcode/issues");
    }));

    if let Err(e) = run().await {
        // Restore terminal before printing error
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
        );
        eprintln!("\nAtomCode error: {:#}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Handle subcommands
    if let Some(cmd) = cli.command {
        return handle_command(cmd).await;
    }

    // Default: start TUI

    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);

    let mut config = if config_path.exists() {
        Config::load(&config_path).unwrap_or_else(|e| {
            eprintln!("Warning: failed to load config ({}), using defaults", e);
            Config {
                default_provider: String::new(),
                default_workdir: None,
                providers: HashMap::new(),
            }
        })
    } else {
        // No config yet — TUI Welcome screen will guide first-run setup
        Config {
            default_provider: String::new(),
            default_workdir: None,
            providers: HashMap::new(),
        }
    };

    let (provider_config, model_name) = if config.providers.is_empty() {
        // No providers configured yet — Welcome screen handles setup.
        // Use a dummy provider; AgentLoop won't be called until user configures one.
        let dummy = ProviderConfig {
            provider_type: "openai".to_string(),
            api_key: Some("not-configured".to_string()),
            model: String::new(),
            base_url: Some("http://localhost:1".to_string()),
            system_prompt: None,
            user_agent: None,
            context_window: default_context_window_for("openai"),
            provider: None,
            context_strategy: Default::default(),
        };
        (dummy, String::new())
    } else {
        if let Some(ref model) = cli.model {
            let provider_name = cli.provider.as_deref().unwrap_or(&config.default_provider);
            if let Some(p) = config.providers.get_mut(provider_name) {
                p.model = model.clone();
            }
        }
        let pc = config.active_provider(cli.provider.as_deref())?.clone();
        let name = pc.model.clone();
        (pc, name)
    };
    let provider = create_provider(&provider_config)?;

    let working_dir = if let Some(d) = cli.dir {
        std::fs::canonicalize(d).unwrap_or_else(|_| std::env::current_dir().unwrap())
    } else {
        // Check if last session was in a different directory (user used /cd last time).
        // If so, offer to resume there. Otherwise use current directory.
        let cwd = std::env::current_dir().unwrap();
        let last_dir_path = atomcode_core::config::Config::config_dir().join("recent_dirs.txt");
        if let Ok(content) = std::fs::read_to_string(&last_dir_path) {
            if let Some(last) = content.lines().next() {
                let last_path = std::path::PathBuf::from(last);
                if last_path != cwd && last_path.exists() {
                    // Last /cd was to a different directory — use it
                    last_path
                } else {
                    cwd
                }
            } else { cwd }
        } else { cwd }
    };

    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(Box::new(ReadFileTool));
    tool_registry.register(Box::new(WriteFileTool));
    tool_registry.register(Box::new(EditFileTool));
    tool_registry.register(Box::new(BashTool));
    tool_registry.register(Box::new(GrepTool));
    tool_registry.register(Box::new(GlobTool));
    tool_registry.register(Box::new(ListDirTool));
    tool_registry.register(Box::new(WebSearchTool));
    tool_registry.register(Box::new(WebFetchTool));
    tool_registry.register(Box::new(SearchReplaceTool));
    let tool_context = ToolContext::new(working_dir.clone());

    // Auto-continue the latest session for this working directory.
    // Same behavior as Claude Code: re-entering a project resumes where you left off.
    // Use --new to force a fresh session.
    let session_to_continue = {
        let session_manager = SessionManager::new(&working_dir);
        match session_manager.latest() {
            Ok(Some(session)) => Some(session),
            _ => None,
        }
    };

    // Start with a fresh conversation each session.
    // Previous session context is injected via build_previous_session_context()
    // from the saved history file — no need to load raw messages.
    // Loading raw messages caused: old model's tool_call format incompatibility,
    // stale file paths from old working directories, and 100+ message context pollution.
    let conversation = Conversation::new();

    let (agent_loop, agent_handle) = AgentLoop::new(
        config.clone(),
        provider,
        tool_registry,
        tool_context.clone(),
        conversation,
    );

    // Headless mode: run without TUI
    if cli.headless {
        let prompt = cli.prompt.clone().ok_or_else(|| {
            anyhow::anyhow!("--prompt is required in headless mode")
        })?;
        return run_headless(agent_loop, agent_handle, prompt, cli.provider.as_deref()).await;
    }

    tokio::spawn(agent_loop.run());
    atomcode_tui::run(config, model_name, agent_handle, tool_context, working_dir, session_to_continue).await
}

/// Run agent in headless mode (no TUI, output to stdout).
async fn run_headless(
    agent_loop: AgentLoop,
    agent_handle: atomcode_core::agent::AgentHandle,
    prompt: String,
    _provider_name: Option<&str>,
) -> Result<()> {
    let (cmd_tx, mut event_rx) = {
        let handle = agent_handle;
        (handle.cmd_tx, handle.event_rx)
    };

    // Spawn agent loop
    tokio::spawn(agent_loop.run());

    // Send the prompt
    cmd_tx.send(AgentCommand::SendMessage(prompt))?;

    // Process events until completion
    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta(text) => {
                print!("{}", text);
                io::stdout().flush()?;
            }
            AgentEvent::ToolCallStarted { name, arguments } => {
                println!("\n[Tool: {}]", name);
                if arguments.len() > 200 {
                    println!("  {}...", &arguments[..200]);
                } else {
                    println!("  {}", arguments);
                }
            }
            AgentEvent::ToolCallResult { name, output, success, duration } => {
                let status = if success { "OK" } else { "FAILED" };
                let dur_ms = duration.as_millis();
                if output.is_empty() {
                    println!("[{}: {} {}ms]", name, status, dur_ms);
                } else if output.len() > 500 {
                    println!("[{}: {} {}ms]\n  {}...\n", name, status, dur_ms, &output[..500]);
                } else {
                    println!("[{}: {} {}ms]\n  {}\n", name, status, dur_ms, output);
                }
            }
            AgentEvent::ApprovalNeeded { tool_name, reason, .. } => {
                println!("\n[Approval Required] {}", tool_name);
                println!("  Reason: {}", reason);
                println!("  [Y] Approve  [A] Always allow  [N] Deny");
                print!("> ");
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                match input.trim().to_lowercase().as_str() {
                    "y" | "yes" => {
                        cmd_tx.send(AgentCommand::ApproveTool)?;
                    }
                    "a" | "always" => {
                        cmd_tx.send(AgentCommand::ApproveToolAlways)?;
                    }
                    _ => {
                        cmd_tx.send(AgentCommand::DenyTool)?;
                    }
                }
            }
            AgentEvent::TokenUsage(usage) => {
                // Silent in headless mode, or optionally show
                eprintln!("[Tokens: {} prompt + {} completion]", usage.prompt_tokens, usage.completion_tokens);
            }
            AgentEvent::PhaseChange(phase) => {
                // Optional: show phase changes
                match phase {
                    atomcode_core::agent::AgentPhase::Idle => {}
                    atomcode_core::agent::AgentPhase::Thinking => {
                        eprintln!("[Thinking...]");
                    }
                    atomcode_core::agent::AgentPhase::CallingTool(name) => {
                        eprintln!("[Executing: {}]", name);
                    }
                    atomcode_core::agent::AgentPhase::WaitingApproval => {}
                }
            }
            AgentEvent::TurnComplete { duration, total_tokens, turn_count, tool_call_count } => {
                println!("\n[Done: {:.1}s, {} tokens, {} turns, {} tool calls]",
                    duration.as_secs_f64(), total_tokens, turn_count, tool_call_count);
                // In headless mode, exit after turn completes
                let _ = cmd_tx.send(AgentCommand::Shutdown);
                break;
            }
            AgentEvent::TurnCancelled { .. } => {
                eprintln!("\n[Cancelled]");
                let _ = cmd_tx.send(AgentCommand::Shutdown);
                break;
            }
            AgentEvent::Error(e) => {
                eprintln!("\n[Error: {}]", e);
            }
            AgentEvent::WorkingDirChanged(new_dir) => {
                eprintln!("[Working directory: {}]", new_dir.display());
            }
            AgentEvent::ContextStats { .. } => {
                // Silent in headless mode
            }
            AgentEvent::SubAgentProgress { file, status } => {
                eprintln!("  ⠴ [{}] {}", file, status);
            }
        }
    }

    Ok(())
}

/// Handle subcommands (login, logout, status)
async fn handle_command(cmd: Commands) -> Result<()> {
    match cmd {
        Commands::Login => {
            let auth = auth::login()?;
            auth::save_auth(&auth)?;
            println!("  Login successful! You can now use AtomCode.");
            Ok(())
        }
        Commands::Logout => {
            auth::logout()?;
            println!("  You have been logged out.");
            Ok(())
        }
        Commands::Status => {
            if let Some(auth) = auth::get_stored_auth() {
                println!("\n  Logged in as: {} ({})", auth.user.username, auth.user.id);
                if let Some(name) = auth.user.name {
                    println!("  Name: {}", name);
                }
                if let Some(email) = auth.user.email {
                    println!("  Email: {}", email);
                }
                println!("  Auth file: {}\n", auth::auth_file_path().display());
            } else {
                println!("\n  Not logged in.");
                println!("  Run 'atomcode login' to authenticate.\n");
            }
            Ok(())
        }
    }
}
