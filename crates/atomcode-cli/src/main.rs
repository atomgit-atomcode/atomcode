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
    /// Login to AtomCode using GitCode OAuth
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

    let config_path = cli.config.unwrap_or_else(Config::default_path);

    let mut config = if config_path.exists() {
        Config::load(&config_path).unwrap_or_else(|e| {
            eprintln!("Warning: failed to load config ({}), using defaults", e);
            Config {
                default_provider: "openai".to_string(),
                default_workdir: None,
                providers: HashMap::new(),
            }
        })
    } else {
        let config = first_run_wizard()?;
        config.save(&config_path)?;
        println!("\nConfig saved to {}\n", config_path.display());
        config
    };

    if config.providers.is_empty() {
        let config = first_run_wizard()?;
        let _ = config.save(&config_path);
        return Ok(());
    }

    if let Some(ref model) = cli.model {
        let provider_name = cli.provider.as_deref().unwrap_or(&config.default_provider);
        if let Some(p) = config.providers.get_mut(provider_name) {
            p.model = model.clone();
        }
    }

    let provider_config = config
        .active_provider(cli.provider.as_deref())?
        .clone();
    let provider = create_provider(&provider_config)?;

    let working_dir = if let Some(d) = cli.dir {
        std::fs::canonicalize(d).unwrap_or_else(|_| std::env::current_dir().unwrap())
    } else if let Some(ref d) = config.default_workdir {
        let p = PathBuf::from(d);
        if p.is_dir() { p } else { std::env::current_dir().unwrap() }
    } else {
        std::env::current_dir().unwrap()
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

    // Derive model name for display in the status bar before giving provider to AgentLoop.
    let model_name = provider_config.model.clone();

    let tool_context = ToolContext::new(working_dir.clone());
    // Load previous session's conversation history for cross-session context.
    // The turn tracker will be rebuilt from the loaded messages, enabling
    // "PREVIOUS SESSION" context injection in the system prompt.
    // Corrupted messages are handled gracefully (Conversation::load backs up + starts fresh).
    let conversation = Conversation::load(&Conversation::history_path());

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
    atomcode_tui::run(config, model_name, agent_handle, tool_context, working_dir).await
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
            AgentEvent::TurnComplete { duration, total_tokens } => {
                println!("\n[Done: {:.1}s, {} tokens]", duration.as_secs_f64(), total_tokens);
                // In headless mode, exit after turn completes
                let _ = cmd_tx.send(AgentCommand::Shutdown);
                break;
            }
            AgentEvent::Error(e) => {
                eprintln!("\n[Error: {}]", e);
            }
            AgentEvent::WorkingDirChanged(new_dir) => {
                eprintln!("[Working directory: {}]", new_dir.display());
            }
        }
    }

    Ok(())
}

fn first_run_wizard() -> Result<Config> {
    println!("Welcome to AtomCode! Let's set up your first provider.\n");
    println!("Select provider:");
    println!("  [1] Claude (Anthropic)");
    println!("  [2] OpenAI");
    println!("  [3] OpenAI Compatible (Deepseek, Qwen, Zhipu, Moonshot...)");
    println!("  [4] Ollama (local)");
    print!("\n> ");
    io::stdout().flush()?;

    let mut choice = String::new();
    io::stdin().read_line(&mut choice)?;
    let choice = choice.trim();

    if choice == "3" {
        return setup_openai_compatible();
    }

    let (name, provider_type, default_model, needs_key, default_base_url) = match choice {
        "1" => ("claude", "claude", "claude-sonnet-4-6", true, None),
        "2" => ("openai", "openai", "gpt-4o", true, Some("https://api.openai.com/v1")),
        "4" => ("ollama", "ollama", "llama3", false, Some("http://localhost:11434")),
        _ => anyhow::bail!("Invalid choice: {}", choice),
    };

    let api_key = if needs_key {
        print!("\nEnter API Key: ");
        io::stdout().flush()?;
        let mut key = String::new();
        io::stdin().read_line(&mut key)?;
        Some(key.trim().to_string())
    } else {
        None
    };

    let mut providers = HashMap::new();
    providers.insert(
        name.to_string(),
        ProviderConfig {
            provider_type: provider_type.to_string(),
            api_key,
            model: default_model.to_string(),
            base_url: default_base_url.map(String::from),
            system_prompt: None,
            context_window: default_context_window_for(provider_type),
        },
    );

    Ok(Config {
        default_provider: name.to_string(),
        default_workdir: None,
        providers,
    })
}

fn setup_openai_compatible() -> Result<Config> {
    println!("\nCommon API base URLs:");
    println!("  Deepseek:  https://api.deepseek.com/v1");
    println!("  Qwen:      https://dashscope.aliyuncs.com/compatible-mode/v1");
    println!("  Zhipu:     https://open.bigmodel.cn/api/paas/v4");
    println!("  Moonshot:  https://api.moonshot.cn/v1");
    println!("  SiliconFlow: https://api.siliconflow.cn/v1");

    print!("\nEnter API Base URL: ");
    io::stdout().flush()?;
    let mut base_url = String::new();
    io::stdin().read_line(&mut base_url)?;
    let base_url = base_url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches("/chat/completions")
        .trim_end_matches('/')
        .to_string();

    print!("Enter API Key: ");
    io::stdout().flush()?;
    let mut api_key = String::new();
    io::stdin().read_line(&mut api_key)?;
    let api_key = api_key.trim().to_string();

    print!("Enter Model name (e.g. deepseek-chat, qwen-plus, glm-4): ");
    io::stdout().flush()?;
    let mut model = String::new();
    io::stdin().read_line(&mut model)?;
    let model = model.trim().to_string();

    let name = url::Url::parse(&base_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| {
            h.split('.').next().unwrap_or("custom").to_string()
        }))
        .unwrap_or_else(|| "custom".to_string());

    let mut providers = HashMap::new();
    providers.insert(
        name.clone(),
        ProviderConfig {
            provider_type: "openai".to_string(),
            api_key: Some(api_key),
            model,
            base_url: Some(base_url),
            system_prompt: None,
            context_window: default_context_window_for("openai"),
        },
    );

    Ok(Config {
        default_provider: name,
        default_workdir: None,
        providers,
    })
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
