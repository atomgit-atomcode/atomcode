pub mod prompt_sections;
pub mod provider;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use atomcode_telemetry::TelemetryConfig;
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
    /// Per-turn datalog settings. Missing from older configs → defaults to
    /// enabled=true, dir=None (writes to `<cwd>/datalog/`).
    ///
    /// `skip_serializing` intentionally suppresses serde's automatic output;
    /// `save()` writes this section manually with explanatory comments and
    /// the default `dir` line commented-out so users can edit the file
    /// without needing to know the field names in advance.
    #[serde(default, skip_serializing)]
    pub datalog: DatalogConfig,
    /// Task-finished notifications. Saved manually with help comments so users
    /// can discover the terminal-first strategy and platform fallbacks.
    #[serde(default, skip_serializing)]
    pub notifications: NotificationConfig,
    /// When true (default), atomcode polls for new releases every hour
    /// while running and stages any newer version it finds. The stage is
    /// applied on the next startup (see `self_update::apply_pending_upgrade`).
    /// Set to `false` to disable auto-staging entirely; `/upgrade` still
    /// works manually. Missing from older configs → defaults to `true`.
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// Every N tool calls, inject a "restate goal / what ruled out / next
    /// output" reflection prompt before the next turn. 0 disables the
    /// checkpoint entirely. Default 7: dogfooding showed 10 is too slow
    /// to catch early "same command, different flag" loops (6-7 tries
    /// before the first checkpoint) while 5 over-injects on normal tasks.
    /// 7 catches early loops in 1-2 retries without disturbing focused work.
    #[serde(default = "default_reflection_cadence")]
    pub reflection_cadence: usize,
    /// Telemetry configuration. Missing from older configs → defaults to
    /// enabled=None (consent-pending), endpoint=None (use the built-in default).
    /// Uses `#[serde(default)]` because `TelemetryConfig` has its own `Default`
    /// impl that matches the no-section-present semantics.
    #[serde(default, skip_serializing)]
    pub telemetry: TelemetryConfig,
    /// LSP integration configuration.
    #[serde(default)]
    pub lsp: LspConfig,
}

/// Controls the per-turn markdown datalog writer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatalogConfig {
    /// When false, `DatalogWriter` becomes a no-op and no files are created.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Where to write datalog files. Accepted forms:
    /// - `None` (or omitted) → `<working_dir>/datalog/` (default, current behavior)
    /// - Absolute path → used as-is, not affected by /cd
    /// - `~/...` → expanded relative to home, not affected by /cd
    /// - Relative path → resolved against working_dir, follows /cd
    #[serde(default)]
    pub dir: Option<String>,
}

/// Controls long-running task completion notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    /// Master switch for all completion notifications.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Only notify when the turn runs for at least this many seconds.
    #[serde(default = "default_notification_min_duration_secs")]
    pub min_duration_secs: u64,
    /// Try terminal-native notification escape sequences first.
    #[serde(default = "default_true")]
    pub terminal: bool,
    /// Fall back to OS-native notifications when terminal protocols are unavailable.
    #[serde(default = "default_true")]
    pub system: bool,
    /// Emit BEL so terminals can play a sound or request attention.
    #[serde(default = "default_true")]
    pub bell: bool,
    /// Best-effort background-only behavior where the terminal protocol supports it.
    #[serde(default = "default_true")]
    pub background_only: bool,
}

/// Controls LSP (Language Server Protocol) integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspConfig {
    /// Master switch for LSP diagnostics.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Automatically detect and start language servers from the built-in registry.
    #[serde(default = "default_true")]
    pub auto_detect: bool,
    /// Custom server configurations keyed by file extension.
    #[serde(default)]
    pub servers: std::collections::HashMap<String, crate::lsp::registry::LspServerConfig>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_detect: true,
            servers: Default::default(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_reflection_cadence() -> usize {
    7
}
fn default_notification_min_duration_secs() -> u64 {
    8
}

impl Default for DatalogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
        }
    }
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_duration_secs: default_notification_min_duration_secs(),
            terminal: true,
            system: true,
            bell: true,
            background_only: true,
        }
    }
}

/// Serialize the `[datalog]` section with help comments so users editing
/// config.toml by hand can discover the options without reading the source.
/// When `dir` is unset, emit a commented-out example; when set, emit it as
/// a real TOML string.
fn render_datalog_section(cfg: &DatalogConfig) -> String {
    let mut out = String::new();
    out.push_str("\n# Per-turn datalog settings. Each turn writes a markdown file\n");
    out.push_str("# (plus a .jsonl of raw LLM requests) into `dir`.\n");
    out.push_str("# - enabled = false        -> disable logging entirely\n");
    out.push_str("# - dir unset (default)    -> writes to <working_dir>/datalog/ (follows /cd)\n");
    out.push_str("# - dir = \"/abs/path\"      -> absolute, fixed (unaffected by /cd)\n");
    out.push_str("# - dir = \"~/sub\"          -> expanded from $HOME, fixed\n");
    out.push_str("# - dir = \"rel/path\"       -> joined with current working_dir, follows /cd\n");
    out.push_str("[datalog]\n");
    out.push_str(&format!("enabled = {}\n", cfg.enabled));
    match &cfg.dir {
        Some(d) => {
            let escaped = d.replace('\\', "\\\\").replace('"', "\\\"");
            out.push_str(&format!("dir = \"{}\"\n", escaped));
        }
        None => {
            // Leave dir unset so behavior stays <cwd>/datalog/. The line below is
            // ONLY an example of the string form — not the actual default.
            out.push_str("# dir = \"~/.atomcode/datalog\"  # example: uncomment to redirect\n");
        }
    }
    out
}

fn render_notifications_section(cfg: &NotificationConfig) -> String {
    let mut out = String::new();
    out.push_str("\n# Long-running task completion notifications.\n");
    out.push_str("# Strategy: terminal-native notifications first (kitty / WezTerm / iTerm2),\n");
    out.push_str(
        "# then OS-native fallback when available (macOS osascript, Linux notify-send).\n",
    );
    out.push_str("# Windows mainly relies on BEL + terminal attention/taskbar flash.\n");
    out.push_str("# `background_only` is best-effort: focus-aware terminal protocols honor it,\n");
    out.push_str("# while some OS fallbacks may still notify even if AtomCode is focused.\n");
    out.push_str("[notifications]\n");
    out.push_str(&format!("enabled = {}\n", cfg.enabled));
    out.push_str(&format!("min_duration_secs = {}\n", cfg.min_duration_secs));
    out.push_str(&format!("terminal = {}\n", cfg.terminal));
    out.push_str(&format!("system = {}\n", cfg.system));
    out.push_str(&format!("bell = {}\n", cfg.bell));
    out.push_str(&format!("background_only = {}\n", cfg.background_only));
    out
}

impl Config {
    /// Context window of the currently-selected default provider.
    /// Falls back to 128_000 when the default_provider is missing or
    /// has no provider entry — matches pre-existing behavior at the
    /// ~5 sites that previously open-coded this lookup.
    pub fn default_context_window(&self) -> usize {
        self.providers
            .get(&self.default_provider)
            .map(|p| p.context_window)
            .unwrap_or(128_000)
    }

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
        // Filter out ephemeral providers (e.g. OAuth /login) — they live in memory only.
        let mut persistent = self.clone();
        persistent.providers.retain(|_, v| !v.ephemeral);
        // If default_provider is ephemeral, don't change the saved default
        if !self
            .providers
            .get(&self.default_provider)
            .map(|p| !p.ephemeral)
            .unwrap_or(true)
        {
            // Restore original default from disk if possible
            if let Ok(disk) = Config::load(path) {
                persistent.default_provider = disk.default_provider;
            }
        }
        let mut content = toml::to_string_pretty(&persistent)?;
        content.push_str(&render_datalog_section(&self.datalog));
        content.push_str(&render_notifications_section(&self.notifications));
        std::fs::write(path, content)?;
        Ok(())
    }

    pub fn active_provider(&self, override_name: Option<&str>) -> Result<&ProviderConfig> {
        // Defence against an accidentally-empty `default_provider` (e.g.
        // an older /logout path wrote "" back to config.toml). Rather
        // than looking up the empty key and failing at startup, fall
        // back to a lexicographically-first provider so the TUI still
        // boots and the user can self-correct via /provider.
        let name: &str = override_name
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.default_provider);
        let name: &str = if name.is_empty() {
            self.providers
                .keys()
                .min()
                .map(String::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("No providers configured — run /codingplan or /provider")
                })?
        } else {
            name
        };
        self.providers
            .get(name)
            .with_context(|| format!("Provider '{}' not found in config", name))
    }

    /// Resolve the atomcode config dir. Pure function for testability —
    /// `config_dir()` is a thin wrapper that injects real env + real home.
    fn resolve_config_dir(env_atomcode_home: Option<String>, home: Option<PathBuf>) -> PathBuf {
        if let Some(p) = env_atomcode_home {
            return PathBuf::from(p);
        }
        home.unwrap_or_else(|| PathBuf::from(".")).join(".atomcode")
    }

    pub fn config_dir() -> PathBuf {
        Self::resolve_config_dir(
            std::env::var("ATOMCODE_HOME")
                .ok()
                .filter(|s| !s.is_empty()),
            crate::tool::real_home_dir(),
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
        let result = Config::resolve_config_dir(None, Some(PathBuf::from("/Users/foo")));
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
        assert_eq!(
            config.providers["ollama"].base_url.as_deref(),
            Some("http://localhost:11434")
        );
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
    fn render_datalog_section_default_has_commented_dir() {
        let rendered = render_datalog_section(&DatalogConfig::default());
        assert!(rendered.contains("[datalog]"));
        assert!(rendered.contains("enabled = true"));
        assert!(rendered.contains("# dir = "));
        assert!(
            !rendered.contains("\ndir = "),
            "default must not emit active dir line"
        );
    }

    #[test]
    fn render_datalog_section_with_dir_emits_real_value() {
        let cfg = DatalogConfig {
            enabled: false,
            dir: Some("~/.atomcode/logs".to_string()),
        };
        let rendered = render_datalog_section(&cfg);
        assert!(rendered.contains("enabled = false"));
        assert!(rendered.contains("dir = \"~/.atomcode/logs\""));
    }

    #[test]
    fn saved_config_roundtrips_datalog() {
        let tmp = std::env::temp_dir().join(format!("atomcode_cfg_rt_{}.toml", std::process::id()));
        let mut cfg = Config {
            default_provider: "p".to_string(),
            default_workdir: None,
            providers: HashMap::new(),
            datalog: DatalogConfig {
                enabled: false,
                dir: Some("/var/log/ac".to_string()),
            },
            notifications: NotificationConfig::default(),
            auto_update: true,
            reflection_cadence: 7,
            telemetry: Default::default(),
            lsp: Default::default(),
        };
        cfg.providers.insert(
            "p".to_string(),
            ProviderConfig {
                provider_type: "openai".to_string(),
                api_key: Some("k".to_string()),
                model: "m".to_string(),
                base_url: None,
                system_prompt: None,
                user_agent: None,
                context_window: 16000,
                max_tokens: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                thinking_enabled: None,
                thinking_budget: None,
                ephemeral: false,
            },
        );
        cfg.save(&tmp).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(text.contains("[datalog]"));
        assert!(text.contains("enabled = false"));
        assert!(text.contains("dir = \"/var/log/ac\""));
        let reloaded = Config::load(&tmp).unwrap();
        assert!(!reloaded.datalog.enabled);
        assert_eq!(reloaded.datalog.dir.as_deref(), Some("/var/log/ac"));
        assert!(reloaded.notifications.enabled);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn render_notifications_section_emits_defaults() {
        let rendered = render_notifications_section(&NotificationConfig::default());
        assert!(rendered.contains("[notifications]"));
        assert!(rendered.contains("enabled = true"));
        assert!(rendered.contains("min_duration_secs = 8"));
        assert!(rendered.contains("background_only = true"));
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

    #[test]
    fn active_provider_falls_back_when_default_is_empty() {
        // Guards against the /logout bug where default_provider got
        // written back as "" — startup must still succeed by falling
        // back to a lexicographically-first provider instead of
        // failing with "Provider '' not found".
        let toml_str = r#"
            default_provider = ""

            [providers.zeta]
            type = "openai"
            api_key = "sk-z"
            model = "gpt-4o"

            [providers.alpha]
            type = "claude"
            api_key = "sk-a"
            model = "claude-opus-4-6"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let provider = config.active_provider(None).unwrap();
        assert_eq!(provider.model, "claude-opus-4-6");
    }

    #[test]
    fn active_provider_ignores_empty_override() {
        let toml_str = r#"
            default_provider = "claude"

            [providers.claude]
            type = "claude"
            api_key = "sk-ant-test"
            model = "claude-opus-4-6"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let provider = config.active_provider(Some("")).unwrap();
        assert_eq!(provider.model, "claude-opus-4-6");
    }

    #[test]
    fn active_provider_errors_with_no_providers_and_empty_default() {
        let toml_str = r#"
            default_provider = ""
            [providers]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let err = config.active_provider(None).unwrap_err();
        assert!(
            err.to_string().contains("No providers configured"),
            "unexpected error: {err}"
        );
    }
}

#[cfg(test)]
mod reflection_config_tests {
    use super::*;

    #[test]
    fn reflection_cadence_defaults_to_seven_when_missing_from_toml() {
        let toml_text = r#"
default_provider = "claude"
[providers]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parses minimal config");
        assert_eq!(cfg.reflection_cadence, 7);
    }

    #[test]
    fn reflection_cadence_zero_means_disabled() {
        let toml_text = r#"
default_provider = "claude"
reflection_cadence = 0
[providers]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parses config with 0");
        assert_eq!(cfg.reflection_cadence, 0);
    }

    #[test]
    fn reflection_cadence_custom_value_is_preserved() {
        let toml_text = r#"
default_provider = "claude"
reflection_cadence = 7
[providers]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parses");
        assert_eq!(cfg.reflection_cadence, 7);
    }

    #[test]
    fn notifications_default_when_missing_from_toml() {
        let toml_text = r#"
default_provider = "claude"
[providers]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parses config");
        assert!(cfg.notifications.enabled);
        assert_eq!(cfg.notifications.min_duration_secs, 8);
        assert!(cfg.notifications.terminal);
        assert!(cfg.notifications.system);
        assert!(cfg.notifications.bell);
        assert!(cfg.notifications.background_only);
    }
}

#[cfg(test)]
mod telemetry_section_tests {
    use super::*;

    #[test]
    fn missing_telemetry_section_uses_defaults() {
        let s = r#"
default_provider = "claude"
[providers]
"#;
        let c: Config = toml::from_str(s).unwrap();
        assert!(c.telemetry.enabled.is_none());
    }

    #[test]
    fn telemetry_section_roundtrip() {
        let s = r#"
default_provider = "claude"
[providers]
[telemetry]
enabled = false
endpoint = "https://test.example/v1"
"#;
        let c: Config = toml::from_str(s).unwrap();
        assert_eq!(c.telemetry.enabled, Some(false));
        assert_eq!(
            c.telemetry.endpoint.as_deref(),
            Some("https://test.example/v1")
        );
    }
}
