//! UI-neutral catalog for safely editable settings.
//!
//! The catalog deliberately contains no model, provider, account, endpoint, or
//! credential fields. Drivers can render it without learning the full `Config`
//! schema, while writes remain document-level patches through `ConfigStore`.
//! The one current-model setting exposed by `/config` uses the explicit helper
//! below instead of entering the static catalog, because its TOML path depends
//! on whether the active selection uses the legacy or account/model schema.

use anyhow::{bail, Result};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    Boolean,
    OptionalBoolean,
    Integer { min: i64, max: i64 },
    Choice(&'static [&'static str]),
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyPolicy {
    ImmediateUi,
    NextTurn,
    AgentReassemble,
    CapabilityReprepare,
    NextStartup,
}

#[derive(Clone, Copy, Debug)]
pub struct SettingSpec {
    pub id: &'static str,
    pub path: &'static [&'static str],
    pub label_en: &'static str,
    pub label_zh: &'static str,
    pub aliases: &'static [&'static str],
    pub kind: SettingKind,
    pub apply: ApplyPolicy,
}

const TODO_EAGERNESS: &[&str] = &["auto", "preferred", "always"];
const THEMES: &[&str] = &["auto", "dark", "light"];
const LANGUAGES: &[&str] = &["auto", "en", "zh_CN"];
const SHELL_GUARD_POLICIES: &[&str] = &["prompt", "strict", "off"];

pub static SETTINGS: &[SettingSpec] = &[
    bool_setting(
        "auto_update",
        &["auto_update"],
        "Auto update",
        "自动更新",
        &["upgrade"],
        ApplyPolicy::NextStartup,
    ),
    bool_setting(
        "keep_interrupted_context",
        &["keep_interrupted_context"],
        "Keep interrupted context",
        "保留中断上下文",
        &["cancel", "undo"],
        ApplyPolicy::AgentReassemble,
    ),
    bool_setting(
        "tools.todo.enabled",
        &["tools", "todo", "enabled"],
        "Todo tool",
        "Todo 工具",
        &["task list"],
        ApplyPolicy::CapabilityReprepare,
    ),
    SettingSpec {
        id: "tools.todo.eager",
        path: &["tools", "todo", "eager"],
        label_en: "Todo eagerness",
        label_zh: "Todo 积极度",
        aliases: &["plan", "always"],
        kind: SettingKind::Choice(TODO_EAGERNESS),
        apply: ApplyPolicy::AgentReassemble,
    },
    SettingSpec {
        id: "coding.max_rounds",
        path: &["coding", "max_rounds"],
        label_en: "Turn max rounds",
        label_zh: "单回合最大轮数",
        aliases: &["rounds"],
        kind: SettingKind::Integer { min: 0, max: 10000 },
        apply: ApplyPolicy::AgentReassemble,
    },
    SettingSpec {
        id: "coding.shell_guard_policy",
        path: &["coding", "shell_guard_policy"],
        label_en: "Shell safety policy",
        label_zh: "Shell 安全策略",
        aliases: &["安全", "凭据", "credential", "shell"],
        kind: SettingKind::Choice(SHELL_GUARD_POLICIES),
        apply: ApplyPolicy::CapabilityReprepare,
    },
    SettingSpec {
        id: "loop_config.max_rounds",
        path: &["loop_config", "max_rounds"],
        label_en: "Loop max rounds",
        label_zh: "循环最大轮数",
        aliases: &["loop"],
        kind: SettingKind::Integer { min: 0, max: 10000 },
        apply: ApplyPolicy::NextTurn,
    },
    SettingSpec {
        id: "subagent.max_concurrent",
        path: &["subagent", "max_concurrent"],
        label_en: "Concurrent subagents",
        label_zh: "并发子代理数",
        aliases: &["task", "agent"],
        kind: SettingKind::Integer { min: 1, max: 64 },
        apply: ApplyPolicy::CapabilityReprepare,
    },
    SettingSpec {
        id: "subagent.max_rounds",
        path: &["subagent", "max_rounds"],
        label_en: "Subagent max rounds",
        label_zh: "子代理最大轮数",
        aliases: &["task", "agent"],
        kind: SettingKind::Integer { min: 0, max: 10000 },
        apply: ApplyPolicy::CapabilityReprepare,
    },
    SettingSpec {
        id: "ui.theme",
        path: &["ui", "theme"],
        label_en: "Theme",
        label_zh: "主题",
        aliases: &["dark", "light"],
        kind: SettingKind::Choice(THEMES),
        apply: ApplyPolicy::NextStartup,
    },
    bool_setting(
        "ui.auto_copy_code_blocks",
        &["ui", "auto_copy_code_blocks"],
        "Copy code blocks",
        "自动复制代码块",
        &["clipboard"],
        ApplyPolicy::NextStartup,
    ),
    bool_setting(
        "ui.ai_session_naming",
        &["ui", "ai_session_naming"],
        "AI session naming",
        "AI 会话命名",
        &["title"],
        ApplyPolicy::AgentReassemble,
    ),
    bool_setting(
        "ui.terminal_status_glyph",
        &["ui", "terminal_status_glyph"],
        "Terminal status glyph",
        "终端状态图标",
        &["tab", "title"],
        ApplyPolicy::ImmediateUi,
    ),
    bool_setting(
        "ui.truncate_resumed_history",
        &["ui", "truncate_resumed_history"],
        "Truncate resumed history",
        "恢复历史截断",
        &["resume", "history", "replay", "完整展示"],
        ApplyPolicy::ImmediateUi,
    ),
    bool_setting(
        "notifications.enabled",
        &["notifications", "enabled"],
        "Notifications",
        "完成通知",
        &["notify"],
        ApplyPolicy::NextTurn,
    ),
    bool_setting(
        "notifications.bell",
        &["notifications", "bell"],
        "Notification bell",
        "通知铃声",
        &["sound"],
        ApplyPolicy::NextTurn,
    ),
    bool_setting(
        "datalog.enabled",
        &["datalog", "enabled"],
        "Datalog",
        "调用日志",
        &["debug", "log"],
        ApplyPolicy::CapabilityReprepare,
    ),
    optional_bool_setting(
        "telemetry.enabled",
        &["telemetry", "enabled"],
        "Telemetry",
        "遥测",
        &["metrics"],
        ApplyPolicy::NextStartup,
    ),
    bool_setting(
        "lsp.enabled",
        &["lsp", "enabled"],
        "LSP code intelligence",
        "LSP 代码智能",
        &["language server"],
        ApplyPolicy::CapabilityReprepare,
    ),
    bool_setting(
        "lsp.auto_detect",
        &["lsp", "auto_detect"],
        "Auto-detect LSP",
        "自动检测 LSP",
        &["language server"],
        ApplyPolicy::CapabilityReprepare,
    ),
    bool_setting(
        "plugin.auto_update_marketplaces",
        &["plugin", "auto_update_marketplaces"],
        "Update plugin marketplaces",
        "更新插件市场",
        &["plugin"],
        ApplyPolicy::NextStartup,
    ),
    SettingSpec {
        id: "language",
        path: &["language"],
        label_en: "Language",
        label_zh: "语言",
        aliases: &["locale", "中文", "english"],
        kind: SettingKind::Choice(LANGUAGES),
        apply: ApplyPolicy::ImmediateUi,
    },
    SettingSpec {
        id: "init_prompt_file",
        path: &["init_prompt_file"],
        label_en: "Custom /init prompt file",
        label_zh: "自定义 /init 提示词文件",
        aliases: &["agents", "AGENTS.md", "初始化"],
        kind: SettingKind::Text,
        apply: ApplyPolicy::NextTurn,
    },
];

const fn bool_setting(
    id: &'static str,
    path: &'static [&'static str],
    label_en: &'static str,
    label_zh: &'static str,
    aliases: &'static [&'static str],
    apply: ApplyPolicy,
) -> SettingSpec {
    SettingSpec {
        id,
        path,
        label_en,
        label_zh,
        aliases,
        kind: SettingKind::Boolean,
        apply,
    }
}

impl SettingSpec {
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.id.to_lowercase().contains(&query)
            || self.label_en.to_lowercase().contains(&query)
            || self.label_zh.to_lowercase().contains(&query)
            || self
                .aliases
                .iter()
                .any(|alias| alias.to_lowercase().contains(&query))
    }

    pub fn value(&self, config: &Config) -> String {
        match self.id {
            "auto_update" => config.auto_update.to_string(),
            "keep_interrupted_context" => config.keep_interrupted_context.to_string(),
            "tools.todo.enabled" => config.tools.todo.enabled.to_string(),
            "tools.todo.eager" => format!("{:?}", config.tools.todo.eager).to_lowercase(),
            "coding.max_rounds" => config.coding.max_rounds.to_string(),
            "coding.shell_guard_policy" => {
                format!("{:?}", config.coding.shell_guard_policy).to_lowercase()
            }
            "loop_config.max_rounds" => config.loop_config.max_rounds.to_string(),
            "subagent.max_concurrent" => config.subagent.max_concurrent.to_string(),
            "subagent.max_rounds" => config.subagent.max_rounds.to_string(),
            "ui.theme" => format!("{:?}", config.ui.theme).to_lowercase(),
            "ui.auto_copy_code_blocks" => config.ui.auto_copy_code_blocks.to_string(),
            "ui.ai_session_naming" => config.ui.ai_session_naming.to_string(),
            "ui.terminal_status_glyph" => config.ui.terminal_status_glyph.to_string(),
            "ui.truncate_resumed_history" => (config.ui.truncate_resumed_history
                && config.ui.history_replay_max_rows != Some(0))
            .to_string(),
            "notifications.enabled" => config.notifications.enabled.to_string(),
            "notifications.bell" => config.notifications.bell.to_string(),
            "datalog.enabled" => config.datalog.enabled.to_string(),
            "telemetry.enabled" => match config.telemetry.enabled {
                None => "auto".to_string(),
                Some(true) => "enabled".to_string(),
                Some(false) => "disabled".to_string(),
            },
            "lsp.enabled" => config.lsp.enabled.to_string(),
            "lsp.auto_detect" => config.lsp.auto_detect.to_string(),
            "plugin.auto_update_marketplaces" => config.plugin.auto_update_marketplaces.to_string(),
            "language" => config
                .language
                .map(|v| v.to_string())
                .unwrap_or_else(|| "auto".into()),
            "init_prompt_file" => config
                .init_prompt_file
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    pub fn patch(&self, document: &mut DocumentMut, input: &str) -> Result<()> {
        // `history_replay_max_rows = 0` was the legacy way to disable the cap.
        // When the user explicitly enables the new switch, remove only that
        // legacy sentinel; positive custom caps remain untouched.
        if self.id == "ui.truncate_resumed_history" {
            let enabled = input
                .parse::<bool>()
                .map_err(|_| anyhow::anyhow!("expected true or false"))?;
            set_path(document, self.path, value(enabled));
            if enabled && integer_at_path(document, &["ui", "history_replay_max_rows"]) == Some(0) {
                remove_path(document, &["ui", "history_replay_max_rows"]);
            }
            return Ok(());
        }
        let item = match self.kind {
            SettingKind::Boolean => value(
                input
                    .parse::<bool>()
                    .map_err(|_| anyhow::anyhow!("expected true or false"))?,
            ),
            SettingKind::OptionalBoolean => match input {
                "auto" => {
                    self.reset(document);
                    return Ok(());
                }
                "enabled" => value(true),
                "disabled" => value(false),
                _ => bail!("expected one of: auto, enabled, disabled"),
            },
            SettingKind::Integer { min, max } => {
                let parsed = input
                    .parse::<i64>()
                    .map_err(|_| anyhow::anyhow!("expected an integer"))?;
                if !(min..=max).contains(&parsed) {
                    bail!("value must be between {min} and {max}");
                }
                value(parsed)
            }
            SettingKind::Choice(values) => {
                if !values.contains(&input) {
                    bail!("expected one of: {}", values.join(", "));
                }
                if self.id == "language" && input == "auto" {
                    self.reset(document);
                    return Ok(());
                }
                value(input)
            }
            SettingKind::Text => {
                let input = input.trim();
                if input.is_empty() {
                    self.reset(document);
                    return Ok(());
                }
                value(input)
            }
        };
        set_path(document, self.path, item);
        Ok(())
    }

    pub fn reset(&self, document: &mut DocumentMut) {
        remove_path(document, self.path);
        if self.id == "ui.truncate_resumed_history"
            && integer_at_path(document, &["ui", "history_replay_max_rows"]) == Some(0)
        {
            remove_path(document, &["ui", "history_replay_max_rows"]);
        }
    }
}

const fn optional_bool_setting(
    id: &'static str,
    path: &'static [&'static str],
    label_en: &'static str,
    label_zh: &'static str,
    aliases: &'static [&'static str],
    apply: ApplyPolicy,
) -> SettingSpec {
    SettingSpec {
        id,
        path,
        label_en,
        label_zh,
        aliases,
        kind: SettingKind::OptionalBoolean,
        apply,
    }
}

fn set_path(document: &mut DocumentMut, path: &[&str], item: Item) {
    let (leaf, parents) = path.split_last().expect("setting paths are non-empty");
    let mut table = document.as_table_mut();
    for key in parents {
        if !table.contains_key(key) || !table[key].is_table() {
            table.insert(key, Item::Table(Table::new()));
        }
        table = table[key].as_table_mut().expect("inserted table");
    }
    table.insert(leaf, item);
}

fn remove_path(document: &mut DocumentMut, path: &[&str]) {
    let (leaf, parents) = path.split_last().expect("setting paths are non-empty");
    let mut table = document.as_table_mut();
    for key in parents {
        let Some(next) = table.get_mut(key).and_then(Item::as_table_mut) else {
            return;
        };
        table = next;
    }
    table.remove(leaf);
}

fn integer_at_path(document: &DocumentMut, path: &[&str]) -> Option<i64> {
    let (leaf, parents) = path.split_last()?;
    let mut table = document.as_table();
    for key in parents {
        table = table.get(key)?.as_table()?;
    }
    table.get(leaf)?.as_integer()
}

/// Read the retry override for one concrete model selection. New-schema model
/// profiles win on an id collision, matching `Config::resolve_model`.
pub fn selection_retry_max_attempts(config: &Config, selection: &str) -> Option<u32> {
    if let Some(model) = config.models.get(selection) {
        return model.retry_max_attempts;
    }
    config
        .providers
        .get(selection)
        .and_then(|provider| provider.retry_max_attempts)
}

/// Patch/reset the active selection's retry override without exposing provider
/// credentials to the generic `/config` catalog. An empty value resets to the
/// per-layer defaults; explicit values must fit the runtime's `u32` contract.
pub fn patch_selection_retry_max_attempts(
    document: &mut DocumentMut,
    selection: &str,
    input: Option<&str>,
) -> Result<()> {
    // The document is authoritative here, not the runtime Config. Runtime-only
    // OAuth/CLI providers intentionally exist in memory but are filtered from
    // disk; using Config membership would materialize an incomplete provider or
    // model table containing only retry_max_attempts. Looking at the document
    // also makes this atomic update safe when another process migrated schemas
    // after the panel opened.
    let contains = |parent: &str| {
        document
            .as_table()
            .get(parent)
            .and_then(Item::as_table)
            .is_some_and(|table| table.contains_key(selection))
    };
    let parent = if contains("models") {
        "models"
    } else if contains("providers") {
        "providers"
    } else {
        bail!("active model selection `{selection}` is temporary or not persisted in config.toml");
    };
    let path = [parent, selection, "retry_max_attempts"];
    let Some(input) = input.map(str::trim).filter(|input| !input.is_empty()) else {
        remove_path(document, &path);
        return Ok(());
    };
    let attempts = input
        .parse::<u32>()
        .map_err(|_| anyhow::anyhow!("expected an integer between 1 and {}", u32::MAX))?;
    if attempts == 0 {
        bail!("value must be at least 1; use 1 to disable automatic retries");
    }
    set_path(document, &path, value(i64::from(attempts)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Provider config, model selection and credentials have their own deliberate UIs and must
    /// never be editable through this generic catalog.
    ///
    /// That is a claim about what a setting IS — its id and path — not about the words it can
    /// be found by. The two were checked together against one blob, which made the catalog
    /// unable to hold a setting that merely *talks about* the forbidden concepts:
    /// `coding.shell_guard_policy` is the guard that protects credential files, and listing
    /// "credential" / "凭据" among its aliases is how a user finds it. Under the old blob check
    /// that alias read as a credential leak and failed the suite.
    ///
    /// So: identity is checked against every word, search text only against the ones that name
    /// a secret's VALUE. No setting needs "api_key" or "token" to be discoverable — those
    /// appearing in a label means a secret is being edited here, whatever the id says.
    #[test]
    fn catalog_excludes_provider_model_and_credentials() {
        const SECRET_VALUE_WORDS: &[&str] = &["api_key", "apikey", "token"];
        const CONCEPT_WORDS: &[&str] = &["provider", "model", "credential"];

        for setting in SETTINGS {
            let identity = std::iter::once(setting.id)
                .chain(setting.path.iter().copied())
                .collect::<Vec<_>>()
                .join(".")
                .to_lowercase();
            for forbidden in SECRET_VALUE_WORDS.iter().chain(CONCEPT_WORDS) {
                assert!(
                    !identity.contains(forbidden),
                    "{} is a {forbidden} setting; it does not belong in the generic catalog",
                    setting.id
                );
            }

            let search_text = std::iter::once(setting.label_en)
                .chain(std::iter::once(setting.label_zh))
                .chain(setting.aliases.iter().copied())
                .collect::<Vec<_>>()
                .join(".")
                .to_lowercase();
            for forbidden in SECRET_VALUE_WORDS {
                assert!(
                    !search_text.contains(forbidden),
                    "{} surfaces {forbidden} in its label or aliases",
                    setting.id
                );
            }
        }
    }

    #[test]
    fn credential_shell_guard_stays_findable_by_the_word_credential() {
        // The concession the split above buys, pinned so a future tightening cannot silently
        // take it back: the setting that decides how hard the shell guard protects credential
        // files must remain searchable by that word, in both languages.
        let guard = SETTINGS
            .iter()
            .find(|s| s.id == "coding.shell_guard_policy")
            .expect("the shell guard policy is in the catalog");
        assert!(guard.aliases.contains(&"credential"));
        assert!(guard.aliases.contains(&"凭据"));
    }

    #[test]
    fn catalog_ids_and_paths_are_unique() {
        let mut ids = std::collections::HashSet::new();
        let mut paths = std::collections::HashSet::new();
        for setting in SETTINGS {
            assert!(
                ids.insert(setting.id),
                "duplicate setting id: {}",
                setting.id
            );
            assert!(
                paths.insert(setting.path.join(".")),
                "duplicate setting path: {}",
                setting.id
            );
        }
    }

    #[test]
    fn patch_and_reset_only_touch_the_setting_path() {
        let setting = SETTINGS
            .iter()
            .find(|s| s.id == "tools.todo.eager")
            .unwrap();
        let mut document = "# heading\nunknown = 1\n".parse::<DocumentMut>().unwrap();
        setting.patch(&mut document, "always").unwrap();
        assert!(document.to_string().contains("eager = \"always\""));
        assert!(document.to_string().contains("# heading"));
        setting.reset(&mut document);
        assert!(!document.to_string().contains("eager"));
        assert!(document.to_string().contains("unknown = 1"));
    }

    #[test]
    fn search_uses_labels_ids_and_aliases() {
        let setting = SETTINGS.iter().find(|s| s.id == "ui.theme").unwrap();
        assert!(setting.matches("theme"));
        assert!(setting.matches("主题"));
        assert!(setting.matches("dark"));
        assert!(!setting.matches("telemetry"));
    }

    #[test]
    fn telemetry_preserves_auto_enabled_disabled_tristate() {
        let setting = SETTINGS
            .iter()
            .find(|setting| setting.id == "telemetry.enabled")
            .unwrap();
        let mut document = DocumentMut::new();
        setting.patch(&mut document, "enabled").unwrap();
        assert!(document.to_string().contains("enabled = true"));
        setting.patch(&mut document, "disabled").unwrap();
        assert!(document.to_string().contains("enabled = false"));
        setting.patch(&mut document, "auto").unwrap();
        assert!(!document.to_string().contains("enabled"));
    }

    #[test]
    fn shell_guard_policy_defaults_to_prompt_and_patches_strict() {
        let setting = SETTINGS
            .iter()
            .find(|setting| setting.id == "coding.shell_guard_policy")
            .unwrap();
        let mut document = DocumentMut::new();
        let defaults = Config::default();
        assert_eq!(setting.value(&defaults), "prompt");
        assert_eq!(setting.apply, ApplyPolicy::CapabilityReprepare);

        setting.patch(&mut document, "strict").unwrap();
        let configured: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(setting.value(&configured), "strict");
        assert!(document
            .to_string()
            .contains("shell_guard_policy = \"strict\""));
    }

    #[test]
    fn shell_guard_policy_reads_legacy_recover_as_prompt() {
        // The retired `recover` value must keep parsing (as `prompt`) so an existing
        // TOML does not break on upgrade.
        let configured: Config =
            toml::from_str("[coding]\nshell_guard_policy = \"recover\"\n").unwrap();
        let setting = SETTINGS
            .iter()
            .find(|setting| setting.id == "coding.shell_guard_policy")
            .unwrap();
        assert_eq!(setting.value(&configured), "prompt");
    }

    #[test]
    fn init_prompt_file_is_editable_and_empty_input_resets_it() {
        let setting = SETTINGS
            .iter()
            .find(|setting| setting.id == "init_prompt_file")
            .unwrap();
        let mut document = DocumentMut::new();
        setting.patch(&mut document, "prompts/init-zh.md").unwrap();
        let configured: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(setting.value(&configured), "prompts/init-zh.md");

        setting.patch(&mut document, "  ").unwrap();
        assert!(!document.to_string().contains("init_prompt_file"));
    }

    #[test]
    fn retry_attempts_patch_the_selected_legacy_provider_and_reset_cleanly() {
        let source = r#"
default_provider = "legacy"
[providers.legacy]
type = "openai"
model = "model-a"
base_url = "https://example.invalid/v1"
"#;
        let mut document = source.parse::<DocumentMut>().unwrap();

        patch_selection_retry_max_attempts(&mut document, "legacy", Some("5")).unwrap();
        let configured: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(selection_retry_max_attempts(&configured, "legacy"), Some(5));

        patch_selection_retry_max_attempts(&mut document, "legacy", None).unwrap();
        let reset: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(selection_retry_max_attempts(&reset, "legacy"), None);
    }

    #[test]
    fn retry_attempts_patch_quoted_new_schema_model_and_reject_zero() {
        let source = r#"
default_model = "account/model"
[provider_accounts.account]
provider = "openai"
[models."account/model"]
account = "account"
model = "model-a"
"#;
        let mut document = source.parse::<DocumentMut>().unwrap();

        patch_selection_retry_max_attempts(&mut document, "account/model", Some("9")).unwrap();
        let configured: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(
            selection_retry_max_attempts(&configured, "account/model"),
            Some(9)
        );
        assert!(
            patch_selection_retry_max_attempts(&mut document, "account/model", Some("0")).is_err()
        );
    }

    #[test]
    fn retry_attempts_never_materialize_runtime_only_selections() {
        let mut document = "default_provider = \"persisted\"\n"
            .parse::<DocumentMut>()
            .unwrap();

        let error =
            patch_selection_retry_max_attempts(&mut document, "oauth-runtime-only", Some("5"))
                .unwrap_err()
                .to_string();

        assert!(error.contains("temporary or not persisted"), "{error}");
        assert!(!document.to_string().contains("oauth-runtime-only"));
        assert!(!document.to_string().contains("retry_max_attempts"));
    }

    #[test]
    fn resumed_history_toggle_preserves_custom_row_cap() {
        let setting = SETTINGS
            .iter()
            .find(|setting| setting.id == "ui.truncate_resumed_history")
            .unwrap();

        let mut document = "[ui]\nhistory_replay_max_rows = 777\n"
            .parse::<DocumentMut>()
            .unwrap();
        let configured: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(setting.value(&configured), "true");

        setting.patch(&mut document, "false").unwrap();
        assert!(document
            .to_string()
            .contains("history_replay_max_rows = 777"));
        assert!(document
            .to_string()
            .contains("truncate_resumed_history = false"));
        let unlimited: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(setting.value(&unlimited), "false");

        setting.patch(&mut document, "true").unwrap();
        assert!(document
            .to_string()
            .contains("history_replay_max_rows = 777"));
        let restored: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(setting.value(&restored), "true");
    }

    #[test]
    fn enabling_resumed_history_truncation_migrates_legacy_zero_cap() {
        let setting = SETTINGS
            .iter()
            .find(|setting| setting.id == "ui.truncate_resumed_history")
            .unwrap();
        let mut document = "[ui]\nhistory_replay_max_rows = 0\n"
            .parse::<DocumentMut>()
            .unwrap();
        let legacy: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(setting.value(&legacy), "false");

        setting.patch(&mut document, "true").unwrap();
        assert!(!document.to_string().contains("history_replay_max_rows"));
        assert!(document
            .to_string()
            .contains("truncate_resumed_history = true"));

        setting.reset(&mut document);
        assert!(!document.to_string().contains("truncate_resumed_history"));
        let reset: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(setting.value(&reset), "true");
    }

    #[test]
    fn runtime_owned_settings_require_reprepare() {
        for id in [
            "keep_interrupted_context",
            "tools.todo.enabled",
            "tools.todo.eager",
            "coding.max_rounds",
            "coding.shell_guard_policy",
            "subagent.max_concurrent",
            "subagent.max_rounds",
            "ui.ai_session_naming",
            "datalog.enabled",
            "lsp.enabled",
            "lsp.auto_detect",
        ] {
            let setting = SETTINGS.iter().find(|setting| setting.id == id).unwrap();
            let expected = match id {
                "tools.todo.enabled"
                | "subagent.max_concurrent"
                | "subagent.max_rounds"
                | "datalog.enabled"
                | "lsp.enabled"
                | "lsp.auto_detect"
                | "coding.shell_guard_policy" => ApplyPolicy::CapabilityReprepare,
                _ => ApplyPolicy::AgentReassemble,
            };
            assert_eq!(setting.apply, expected, "{id}");
        }
    }
}
