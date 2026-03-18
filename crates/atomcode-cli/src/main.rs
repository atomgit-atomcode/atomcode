use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use atomcode_core::config::provider::ProviderConfig;
use atomcode_core::config::Config;
use atomcode_core::provider::create_provider;

#[derive(Parser)]
#[command(name = "atomcode", version = "0.1.0", about = "AI coding assistant in your terminal")]
struct Cli {
    /// Provider to use (overrides config default)
    #[arg(long)]
    provider: Option<String>,

    /// Model to use (overrides config provider model)
    #[arg(long)]
    model: Option<String>,

    /// Path to config file
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(Config::default_path);

    let mut config = if config_path.exists() {
        Config::load(&config_path)?
    } else {
        let config = first_run_wizard()?;
        config.save(&config_path)?;
        println!("\nConfig saved to {}\n", config_path.display());
        config
    };

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

    atomcode_tui::run(config, provider).await
}

fn first_run_wizard() -> Result<Config> {
    println!("Welcome to AtomCode! Let's set up your first provider.\n");
    println!("Select provider:");
    println!("  [1] Claude (Anthropic)");
    println!("  [2] OpenAI");
    println!("  [3] Ollama (local)");
    print!("\n> ");
    io::stdout().flush()?;

    let mut choice = String::new();
    io::stdin().read_line(&mut choice)?;
    let choice = choice.trim();

    let (name, provider_type, default_model, needs_key, default_base_url) = match choice {
        "1" => ("claude", "claude", "claude-sonnet-4-6", true, None),
        "2" => ("openai", "openai", "gpt-4o", true, Some("https://api.openai.com/v1")),
        "3" => ("ollama", "ollama", "llama3", false, Some("http://localhost:11434")),
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
        },
    );

    Ok(Config {
        default_provider: name.to_string(),
        providers,
    })
}
