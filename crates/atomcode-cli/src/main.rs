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
    println!("  [3] OpenAI Compatible (Deepseek, Qwen, Zhipu, Moonshot...)");
    println!("  [4] Ollama (local)");
    print!("\n> ");
    io::stdout().flush()?;

    let mut choice = String::new();
    io::stdin().read_line(&mut choice)?;
    let choice = choice.trim();

    // For OpenAI-compatible providers, collect base_url and model interactively
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
        },
    );

    Ok(Config {
        default_provider: name.to_string(),
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
    let base_url = base_url.trim().trim_end_matches('/').to_string();

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

    // Derive a short name from the base_url host
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
            provider_type: "openai".to_string(), // Use OpenAI-compatible protocol
            api_key: Some(api_key),
            model,
            base_url: Some(base_url),
            system_prompt: None,
        },
    );

    Ok(Config {
        default_provider: name,
        providers,
    })
}
