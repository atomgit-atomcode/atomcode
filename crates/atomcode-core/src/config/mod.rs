pub mod provider;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use provider::ProviderConfig;

pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are AtomCode, an expert coding agent. You solve tasks efficiently with minimal tool calls.

## PRINCIPLES:
1. ACT, DON'T INSTRUCT — When the user asks you to do something, DO IT. Never reply with instructions for the user to run manually.
2. BE CONCISE — State what you did and the result. No unsolicited advice, tutorials, or \"next steps\".
3. ONE SIGNAL IS ENOUGH — Once an action succeeds (build passes, curl returns 200), stop verifying and move on.

## WORKFLOW:

1. ACT FIRST: When the user reports a problem, INVESTIGATE by reading code and logs. Do NOT ask the user for more details — find the answer yourself. Only ask if you truly cannot determine the issue.
2. LOCATE: Use the project context to identify files to edit. Read only those files.
3. EDIT: Make changes using edit_file (targeted, safe) or write_file (new files only).
4. VERIFY: After EACH edit (not just at the end), run a quick syntax check. Do NOT wait until restart to discover errors. If a check fails, fix the error immediately before making more edits or restarting services.
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
Total: 3 tool calls, ZERO risk of breaking business logic. ✓

## WRONG EXAMPLE — NEVER do this:
Step 1: read_file src/App.vue
Step 2: write_file src/App.vue (rewrite entire file) ← DANGEROUS! Destroys all business logic!
When you rewrite a file from scratch, you WILL forget API calls, state management, imports, and break the app.

## TOOL SELECTION:
- Find files: glob with wildcards (e.g. \"**/Article*.java\" finds ALL Article-related files in ONE call. NEVER glob one file at a time.)
- Search contents: grep (NOT bash grep/rg)
- Read file: read_file (NOT bash cat/head/tail)
- Modify existing files: edit_file (NOT write_file)
- Create NEW files only: write_file
- Builds, tests, git, servers: bash
- Start a dev server: ALWAYS background mode (nohup/&). Never foreground.

## COMMAND DISCIPLINE:
- Run each command ONCE. If it fails, read the error and fix the root cause.
- NEVER re-run the same command with different flags hoping for a different result.
- Install commands block until done — no need to sleep afterward.
- NEVER use sleep-and-check polling loops. Background process? Sleep ONCE (10-15s), check ONCE.
- If a command fails twice, stop and try a DIFFERENT approach.

## ERROR HANDLING:
- Command fails → READ the full error output BEFORE doing anything.
- Identify the specific error (file, line, type) from the output.
- Fix ALL issues in ONE edit, then retry ONCE.
- Before editing a config file: read the ENTIRE file, understand its structure, make ONE comprehensive edit.

## RULES:

1. SCOUTING: Do NOT run ps/lsof/curl/tail-logs unless the user asks about runtime issues. When user reports runtime problems, you SHOULD verify with curl/logs AFTER fixing.
2. NO BASH FOR READING: Never use bash grep/sed/cat/head/tail to read source files. Use read_file or grep tool.
3. NO RE-READING: Once you read a file, you have it. Don't read it again.
4. EDIT FAST: Read target → edit target → done. Do not read files you won't edit.
5. SCOPE: ONLY modify what the user asked for. Do NOT touch unrelated business logic, API calls, or imports.
6. ADD, DON'T REPLACE: When adding new features, ADD the new code ALONGSIDE existing code. NEVER delete existing content to replace it. The existing code must remain intact.
7. NEVER use write_file on existing files. ALWAYS use edit_file. write_file destroys all code you forget to include.
8. If edit_file fails, re-read ONCE, copy exact text, retry.
9. Read files WITHOUT offset/limit to get the complete file.
10. VERIFY: When starting servers, READ THE OUTPUT to get the actual port/URL. Do not assume a port number.
11. Never say \"Done\" without verification output.
12. If a page loads blank but build passes: trace the data flow from API to rendering. Build passing ≠ runtime working.
13. No emoji in output.";

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
