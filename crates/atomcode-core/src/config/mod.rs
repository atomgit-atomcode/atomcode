pub mod provider;
pub mod prompt_sections;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use provider::ProviderConfig;

// DEFAULT_SYSTEM_PROMPT removed — single source of truth is now
// config/prompt_sections.rs::UNIFIED_PROMPT (~500 tok).
// Do NOT add prompt rules here. Edit prompt_sections.rs instead.

/// Windows-specific rules appended to the system prompt.
/// Only injected on Windows builds — macOS/Linux never see these.
#[allow(clippy::needless_raw_string_hashes)]
pub const WINDOWS_RULES: &str = r##"\

## WINDOWS PLATFORM RULES:

- Bash runs via cmd.exe, NOT WSL. Use Windows syntax: dir (not ls), where (not which), type (not cat).
- Path separators: use \\ in commands. Example: cd src\\components
- Install tools: use winget, choco, or direct download. NOT apt/brew.
- Check tools: where <tool_name> (not which).
- PowerShell: for complex scripts, use powershell -Command "..."
- Virtual environments: check for Scripts\\ subdirectory (not bin/)"##;

/// macOS-specific rules (minimal — macOS is the primary dev platform).
pub const MACOS_RULES: &str = "";

/// Linux-specific rules.
pub const LINUX_RULES: &str = "";

/// Get platform-specific rules for the current OS.
pub fn platform_rules() -> &'static str {
    if cfg!(target_os = "windows") {
        WINDOWS_RULES
    } else if cfg!(target_os = "macos") {
        MACOS_RULES
    } else {
        LINUX_RULES
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub default_provider: String,
    /// Default working directory. Saved on /cd, restored on startup.
    pub default_workdir: Option<String>,
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

    /// Resolve the atomcode config dir. Pure function for testability —
    /// `config_dir()` is a thin wrapper that injects real env + real home.
    fn resolve_config_dir(
        env_atomcode_home: Option<String>,
        home: Option<PathBuf>,
    ) -> PathBuf {
        if let Some(p) = env_atomcode_home {
            return PathBuf::from(p);
        }
        home.unwrap_or_else(|| PathBuf::from("."))
            .join(".atomcode")
    }

    pub fn config_dir() -> PathBuf {
        Self::resolve_config_dir(
            std::env::var("ATOMCODE_HOME").ok().filter(|s| !s.is_empty()),
            dirs::home_dir(),
        )
    }

    pub fn default_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_config_dir_uses_env_when_set() {
        let result = Config::resolve_config_dir(
            Some("/tmp/custom-atomcode-home".to_string()),
            Some(PathBuf::from("/Users/foo")),
        );
        assert_eq!(result, PathBuf::from("/tmp/custom-atomcode-home"));
    }

    #[test]
    fn test_resolve_config_dir_falls_back_to_home() {
        let result = Config::resolve_config_dir(
            None,
            Some(PathBuf::from("/Users/foo")),
        );
        assert_eq!(result, PathBuf::from("/Users/foo/.atomcode"));
    }

    #[test]
    fn test_resolve_config_dir_falls_back_to_dot_when_no_home() {
        let result = Config::resolve_config_dir(None, None);
        assert_eq!(result, PathBuf::from("./.atomcode"));
    }

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
