use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Set to `true` at the start of `run_headless` so the panic hook and the
/// top-level error handler can skip TUI cleanup. In headless mode we never
/// entered the alternate screen, so calling `LeaveAlternateScreen` would
/// emit `\x1b[?1049l` to stdout and corrupt pipe-friendly output.
static HEADLESS_MODE: AtomicBool = AtomicBool::new(false);

/// Restore terminal state if (and only if) we ever entered TUI mode.
/// No-op in headless mode — see [`HEADLESS_MODE`].
fn restore_terminal_if_tui() {
    if HEADLESS_MODE.load(Ordering::Relaxed) {
        return;
    }
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
    );
}

/// Resolve the working directory at startup. **Always** uses the current
/// working directory unless the user explicitly passed `-C / --dir`.
///
/// We deliberately do **not** read `~/.atomcode/recent_dirs.txt` (or any other
/// "remembered" path). The previous implementation silently substituted the
/// first entry of recent_dirs for the user's cwd, which made commands like
/// `atomcode -p "describe this project"` operate on whatever directory the
/// TUI happened to visit last — a violation of least surprise. recent_dirs
/// remains a TUI picker convenience only; it must never override cwd.
fn resolve_working_dir(cli_dir: Option<PathBuf>) -> PathBuf {
    if let Some(d) = cli_dir {
        std::fs::canonicalize(&d).unwrap_or(d)
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// Truncate a string to at most `max_chars` *characters* (not bytes), replacing
/// any newlines with spaces and appending "..." when truncated.
///
/// Used for headless-mode log lines on stderr. **Counts characters, not bytes**,
/// so multi-byte UTF-8 (e.g. CJK) is safe — `&s[..N]` would panic when N falls
/// inside a multi-byte char.
fn truncate_log_line(s: &str, max_chars: usize) -> String {
    let single_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if single_line.chars().count() > max_chars {
        let head: String = single_line.chars().take(max_chars).collect();
        format!("{}...", head)
    } else {
        single_line
    }
}

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("ATOMCODE_BUILD_ID"),
    env!("ATOMCODE_BUILD_DIRTY"),
    ")"
);

#[derive(Parser)]
#[command(name = "atomcode", version = VERSION, about = "AI coding assistant in your terminal")]
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

    /// Prompt to run in headless (non-interactive) mode. If omitted, launches the TUI.
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// Show tool calls, token usage, and turn summary on stderr (headless mode only).
    /// Without this flag, headless output is the assistant reply only — Claude Code -p style.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Maximum number of LLM turns before the agent loop is force-stopped.
    /// Bounds context accumulation on long-running tasks (e.g. SWE-bench eval).
    /// Default: unbounded — the agent stops naturally when the model returns
    /// no tool calls or when the step budget (tool-call cap) is reached.
    #[arg(long)]
    max_turns: Option<usize>,
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
        restore_terminal_if_tui();
        eprintln!("\nAtomCode crashed: {}", info);
        if let Some(location) = info.location() {
            eprintln!("  at {}:{}:{}", location.file(), location.line(), location.column());
        }
        eprintln!("\nPlease report this at: https://github.com/atomcode/atomcode/issues");
    }));

    match run().await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            restore_terminal_if_tui();
            eprintln!("\nAtomCode error: {:#}", e);
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<i32> {
    let cli = Cli::parse();

    // Handle subcommands
    if let Some(cmd) = cli.command {
        return handle_command(cmd).await.map(|_| 0);
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

    let working_dir = resolve_working_dir(cli.dir.clone());

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

    let (mut agent_loop, agent_handle) = AgentLoop::new(
        config.clone(),
        provider,
        tool_registry,
        tool_context.clone(),
        conversation,
    );
    agent_loop.set_max_turns(cli.max_turns);

    // Headless mode: -p / --prompt triggers non-interactive execution.
    if let Some(prompt) = cli.prompt.clone() {
        return run_headless(agent_loop, agent_handle, prompt, cli.provider.as_deref(), cli.verbose).await;
    }

    tokio::spawn(agent_loop.run());
    atomcode_tui::run(config, model_name, agent_handle, tool_context, working_dir, session_to_continue).await?;
    Ok(0)
}

/// Run agent in headless mode (pipe-friendly: stdout = LLM text only,
/// logs/diagnostics → stderr, approvals auto-denied).
///
/// `verbose=false` (default): Claude Code -p style — only the assistant reply
/// reaches the user. Tool calls, token usage, and turn summary are silent.
/// Errors, approval denials, and cancellations are still surfaced on stderr.
///
/// `verbose=true`: also emit tool calls, token usage, [done] summary, working
/// dir changes, and sub-agent progress on stderr.
async fn run_headless(
    agent_loop: AgentLoop,
    agent_handle: atomcode_core::agent::AgentHandle,
    prompt: String,
    _provider_name: Option<&str>,
    verbose: bool,
) -> Result<i32> {
    // Tell the panic hook / error path to skip TUI cleanup — we never enter
    // the alternate screen here, so LeaveAlternateScreen would corrupt stdout.
    HEADLESS_MODE.store(true, Ordering::Relaxed);

    let (cmd_tx, mut event_rx) = {
        let handle = agent_handle;
        (handle.cmd_tx, handle.event_rx)
    };

    tokio::spawn(agent_loop.run());
    cmd_tx.send(AgentCommand::SendMessage(prompt))?;

    let mut exit_code: i32 = 0;
    let mut had_denial = false;
    let mut last_text_ended_with_newline = true;

    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta(text) => {
                if !text.is_empty() {
                    last_text_ended_with_newline = text.ends_with('\n');
                }
                print!("{}", text);
                io::stdout().flush()?;
            }
            AgentEvent::ToolCallStarted { name, arguments } => {
                if verbose {
                    let args = truncate_log_line(&arguments, 200);
                    eprintln!("[tool→ {} args={}]", name, args);
                }
            }
            AgentEvent::ToolCallResult { name, output, success, duration } => {
                if verbose {
                    let status = if success { "OK" } else { "FAILED" };
                    let dur_ms = duration.as_millis();
                    let trimmed = output.trim_end();
                    if trimmed.is_empty() {
                        eprintln!("[tool← {} {} {}ms]", name, status, dur_ms);
                    } else {
                        let snippet = truncate_log_line(trimmed, 500);
                        eprintln!("[tool← {} {} {}ms] {}", name, status, dur_ms, snippet);
                    }
                }
            }
            AgentEvent::ApprovalNeeded { tool_name, reason, .. } => {
                // Always shown — security signal must not be silent.
                eprintln!("[approval-denied] tool={} reason={}", tool_name, reason);
                cmd_tx.send(AgentCommand::DenyTool)?;
                had_denial = true;
            }
            AgentEvent::TokenUsage(usage) => {
                if verbose {
                    eprintln!("[tokens] prompt={} completion={}", usage.prompt_tokens, usage.completion_tokens);
                }
            }
            AgentEvent::PhaseChange(_) => {
                // Silent in headless mode (in both default and verbose).
            }
            AgentEvent::TurnComplete { duration, total_tokens, turn_count, tool_call_count, stop_reason } => {
                // Always ensure stdout ends with a newline so downstream parsers see a clean line.
                if !last_text_ended_with_newline {
                    println!();
                    io::stdout().flush()?;
                }
                if verbose {
                    // Natural completion stays silent on the stop reason to
                    // preserve the familiar Claude Code -p [done] format.
                    // Budget-enforced / error / cancel truncation gets an
                    // explicit `stopped=<tag>` suffix so eval runners and
                    // humans can tell "natural end" from "we hit a limit".
                    let suffix = match stop_reason {
                        atomcode_core::agent::TurnStopReason::Natural => String::new(),
                        other => format!(" stopped={}", other.as_tag()),
                    };
                    eprintln!("[done] {:.1}s tokens={} turns={} tool_calls={}{}",
                        duration.as_secs_f64(), total_tokens, turn_count, tool_call_count, suffix);
                }
                let _ = cmd_tx.send(AgentCommand::Shutdown);
                break;
            }
            AgentEvent::TurnCancelled { .. } => {
                // Always shown — user needs to know cancellation happened.
                eprintln!("[cancelled]");
                exit_code = 130;
                let _ = cmd_tx.send(AgentCommand::Shutdown);
                break;
            }
            AgentEvent::Error(e) => {
                // Always shown — errors are not noise.
                eprintln!("[error] {}", e);
                exit_code = 1;
                let _ = cmd_tx.send(AgentCommand::Shutdown);
                break;
            }
            AgentEvent::WorkingDirChanged(new_dir) => {
                if verbose {
                    eprintln!("[cwd] {}", new_dir.display());
                }
            }
            AgentEvent::ContextStats { .. } => {
                // Silent in headless mode
            }
            AgentEvent::SubAgentProgress { file, status } => {
                if verbose {
                    eprintln!("[sub-agent] {} {}", file, status);
                }
            }
        }
    }

    // Priority: Error(1) > Denial(2) > 0; TurnCancelled(130) is absolute.
    if exit_code == 0 && had_denial {
        exit_code = 2;
    }

    Ok(exit_code)
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

#[cfg(test)]
mod tests {
    use super::{resolve_working_dir, truncate_log_line};
    use std::path::PathBuf;

    #[test]
    fn ascii_short_unchanged() {
        assert_eq!(truncate_log_line("hello", 10), "hello");
    }

    #[test]
    fn ascii_long_truncated_with_ellipsis() {
        assert_eq!(truncate_log_line("0123456789abcdef", 10), "0123456789...");
    }

    #[test]
    fn newlines_become_spaces() {
        assert_eq!(truncate_log_line("a\nb\nc", 10), "a b c");
    }

    #[test]
    fn mixed_ascii_cjk_truncates_at_char_boundary() {
        // 8 chars: ['a','b','c','计','算','d','e','f']; max 5 → "abc计算..."
        assert_eq!(truncate_log_line("abc计算def", 5), "abc计算...");
    }

    /// Regression test for panic at `crates/atomcode-cli/src/main.rs:272:42`:
    /// "byte index 500 is not a char boundary; it is inside '计' (bytes 498..501)".
    /// Triggered when ToolCallResult output was a CJK-heavy string > 500 bytes
    /// and the old code did `trimmed[..500]` (byte slice). Pure CJK at 3 bytes
    /// per char means almost any 500-byte cut lands inside a multi-byte char.
    #[test]
    fn cjk_truncation_does_not_panic() {
        let s: String = "计算".repeat(500); // 1000 chars, 3000 bytes
        let result = truncate_log_line(&s, 500);
        assert_eq!(result.chars().count(), 503); // 500 + "..."
        assert!(result.ends_with("..."));
    }

    /// Regression test for cwd-override bug: when no `-C` is given, working dir
    /// must equal `std::env::current_dir()`. Old code silently substituted the
    /// first line of `~/.atomcode/recent_dirs.txt`, breaking `atomcode -p` from
    /// any directory that wasn't the TUI's last-visited project.
    #[test]
    fn resolve_working_dir_uses_cwd_when_no_cli_dir() {
        let expected = std::env::current_dir().unwrap();
        assert_eq!(resolve_working_dir(None), expected);
    }

    #[test]
    fn resolve_working_dir_honors_cli_dir() {
        let temp = std::env::temp_dir();
        let canon = std::fs::canonicalize(&temp).unwrap_or(temp.clone());
        assert_eq!(resolve_working_dir(Some(temp)), canon);
    }

    #[test]
    fn resolve_working_dir_falls_back_to_input_when_canonicalize_fails() {
        // Use a non-existent path so canonicalize() returns Err and the
        // function falls back to the raw input rather than panicking.
        let bogus = PathBuf::from("/nonexistent/atomcode-test-path-xyzzy");
        assert_eq!(resolve_working_dir(Some(bogus.clone())), bogus);
    }
}
