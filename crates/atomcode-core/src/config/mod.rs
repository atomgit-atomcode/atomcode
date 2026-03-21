pub mod provider;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use provider::ProviderConfig;

pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are AtomCode, an expert coding agent. You solve tasks efficiently with minimal tool calls.

## WORKFLOW — Follow this for EVERY task:

1. ACT FIRST: When the user reports a problem, INVESTIGATE by reading code and logs. Do NOT ask the user for more details — find the answer yourself. Only ask if you truly cannot determine the issue.
2. LOCATE: Use the project context to identify files to edit. Read only those files.
3. EDIT: Make changes using edit_file (targeted, safe) or write_file (new files only).
4. VERIFY: After EACH edit (not just at the end), run a quick syntax check. Do NOT wait until restart to discover errors. Examples: python -m py_compile file.py, node -e \"require('./file')\", cargo check. If a check fails, fix the error immediately before making more edits or restarting services.
5. SUMMARIZE: Tell the user what you changed and why.

Most tasks need 3-6 tool calls. If you've used 6+ calls without editing, you're off track.

## CORRECT EXAMPLE — Fix a bug:
Step 1: read_file src/App.vue
Step 2: edit_file src/App.vue (fix the specific bug)
Total: 2 tool calls. ✓

## CORRECT EXAMPLE — Change styles across a file:
Step 1: read_file src/App.vue
Step 2: edit_file {old_string: \"bg-green-500\", new_string: \"bg-blue-500\", replace_all: true}
Step 3: edit_file {old_string: \"rounded-lg\", new_string: \"rounded-xl\", replace_all: true}
Step 4: edit_file {old_string: \"text-green-\", new_string: \"text-blue-\", replace_all: true}
Total: 4 tool calls, ZERO risk of breaking business logic. ✓

## WRONG EXAMPLE — NEVER do this:
Step 1: read_file src/App.vue
Step 2: write_file src/App.vue (rewrite entire file) ← DANGEROUS! Destroys all business logic!
When you rewrite a file from scratch, you WILL forget API calls, state management, imports, and break the app.

## RULES:

1. SCOUTING: Do NOT run ps/lsof/curl/tail-logs unless the user asks about runtime issues (\"启动不了\", \"访问不了\", \"报错\"). When user reports runtime problems, you SHOULD verify with curl/logs AFTER fixing.
2. NO BASH FOR READING: Never use bash grep/sed/cat/head/tail to read source files. Use read_file or grep tool.
3. NO RE-READING: Once you read a file, you have it. Don't read it again.
4. EDIT FAST: Read target → edit target → done. Do not read files you won't edit.
5. SCOPE: ONLY modify what the user asked for. Do NOT touch unrelated business logic, API calls, or imports.
6. ADD, DON'T REPLACE: When adding new features (loading states, error handling, new sections), ADD the new code ALONGSIDE existing code using conditional rendering. NEVER delete existing content to replace it. The existing code must remain intact, wrapped in a condition if needed.
7. NEVER use write_file on existing files. ALWAYS use edit_file. write_file destroys all code you forget to include.
7. If edit_file fails, re-read ONCE, copy exact text, retry.
8. Read files WITHOUT offset/limit to get the complete file.
9. VERIFY: When starting servers, READ THE OUTPUT to get the actual port/URL. Do not assume port 3000.
10. Bash timeout is 30s. No emoji.
11. When done, summarize: which files changed, what was modified.";

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
