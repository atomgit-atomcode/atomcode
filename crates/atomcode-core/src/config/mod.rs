pub mod provider;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use provider::ProviderConfig;

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are AtomCode, an AI coding assistant running in a terminal. Be concise and helpful.

You have access to tools: read_file, write_file, edit_file, and bash. Use them to help the user with coding tasks.

IMPORTANT:
- All file paths should be relative to the working directory unless the user specifies an absolute path.
- When using bash, commands execute in the working directory.
- Prefer edit_file over write_file when modifying existing files — it does precise string replacement instead of rewriting the whole file.
- edit_file requires old_string to be unique in the file. Include enough surrounding context to ensure uniqueness.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub default_provider: String,
    pub providers: HashMap<String, ProviderConfig>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config: {}", path.display()))?;
        let config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config: {}", path.display()))?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    pub fn active_provider(&self, override_name: Option<&str>) -> Result<&ProviderConfig> {
        let name = override_name.unwrap_or(&self.default_provider);
        self.providers
            .get(name)
            .with_context(|| format!("Provider '{}' not found in config", name))
    }

    pub fn config_dir() -> std::path::PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".atomcode")
    }

    pub fn default_path() -> std::path::PathBuf {
        Self::config_dir().join("config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let toml_str = r#"
            default_provider = "claude"

            [providers.claude]
            type = "claude"
            api_key = "sk-ant-test"
            model = "claude-opus-4-6"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_provider, "claude");
        assert_eq!(config.providers.len(), 1);
        let p = &config.providers["claude"];
        assert_eq!(p.provider_type, "claude");
        assert_eq!(p.api_key.as_deref(), Some("sk-ant-test"));
        assert_eq!(p.model, "claude-opus-4-6");
    }

    #[test]
    fn test_parse_multi_provider_config() {
        let toml_str = r#"
            default_provider = "openai"

            [providers.claude]
            type = "claude"
            api_key = "sk-ant-test"
            model = "claude-opus-4-6"

            [providers.openai]
            type = "openai"
            api_key = "sk-test"
            model = "gpt-4o"
            base_url = "https://api.openai.com/v1"

            [providers.ollama]
            type = "ollama"
            model = "llama3"
            base_url = "http://localhost:11434"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_provider, "openai");
        assert_eq!(config.providers.len(), 3);
        assert_eq!(config.providers["ollama"].base_url.as_deref(), Some("http://localhost:11434"));
        assert!(config.providers["ollama"].api_key.is_none());
    }

    #[test]
    fn test_get_active_provider_config() {
        let toml_str = r#"
            default_provider = "claude"

            [providers.claude]
            type = "claude"
            api_key = "sk-ant-test"
            model = "claude-opus-4-6"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let provider = config.active_provider(None).unwrap();
        assert_eq!(provider.model, "claude-opus-4-6");
    }

    #[test]
    fn test_override_provider() {
        let toml_str = r#"
            default_provider = "claude"

            [providers.claude]
            type = "claude"
            api_key = "sk-ant-test"
            model = "claude-opus-4-6"

            [providers.openai]
            type = "openai"
            api_key = "sk-test"
            model = "gpt-4o"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let provider = config.active_provider(Some("openai")).unwrap();
        assert_eq!(provider.model, "gpt-4o");
    }
}
