pub mod instructions;
pub mod memory;
pub mod offline;
pub mod prompt_sections;
pub mod provider;
pub mod provider_preset;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::proxy::ProxyConfig;
use atomcode_telemetry::TelemetryConfig;
use provider::{ModelProfileConfig, ProviderAccountConfig, ProviderConfig, ResolvedModelConfig};

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

/// `[coding]` table. Turn-level knobs for the main coding agent. `max_rounds` is
/// the per-turn round cap (the interactive checkpoint threshold); `0` = unbounded.
/// Env `ATOMCODE_TURN_MAX_ROUNDS` overrides this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodingConfig {
    pub max_rounds: u32,
}
impl Default for CodingConfig {
    fn default() -> Self {
        Self { max_rounds: 200 }
    }
}

/// /loop command configuration. Persisted as the `[loop_config]` table
/// (NOT `[loop]` — `loop` is a Rust keyword and is rejected by toml_edit).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopConfig {
    /// Hard cap on /loop iterations (both modes) before auto-stop; `0` is unbounded.
    pub max_rounds: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self { max_rounds: 100 }
    }
}

/// `[subagent]` execution policy for the `task` subagent tool.
///
/// `max_concurrent` and `max_rounds` are the LIVE knobs: `coding::parts` reads them via
/// `subagent_runtime_knobs` and wires them into `TaskTool`.
/// The tool's master ON/OFF is the env gate `ATOMCODE_SUBAGENT`
/// (default ON, opt out with `ATOMCODE_SUBAGENT=0`) — NOT `enabled` here; `enabled`,
/// `initial_turns`, and `max_turns` are vestigial from the retired `parallel_edit` dispatch
/// path and are not currently consulted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SubAgentConfig {
    /// Vestigial: the live master switch is the env gate `ATOMCODE_SUBAGENT` (default ON),
    /// not this field. Kept for config back-compat.
    pub enabled: bool,
    /// Vestigial (retired resilience path); not currently read.
    pub initial_turns: usize,
    /// Vestigial (retired resilience path); not currently read.
    pub max_turns: usize,
    /// Max parallel subagents the `task` tool runs at once (floored to 1). Default 3.
    pub max_concurrent: usize,
    /// Deprecated compatibility field. Subtasks no longer have a total wall-clock limit;
    /// provider idle timeouts, `max_rounds`, and explicit cancellation own liveness.
    pub timeout_secs: u64,
    /// Per-subtask model-round high-water mark. Default 200; `0` means unbounded.
    /// Overridden by `ATOMCODE_SUBAGENT_MAX_ROUNDS` when set.
    pub max_rounds: u32,
}

impl Default for SubAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            initial_turns: 4,
            max_turns: 12,
            max_concurrent: 3,
            // Retained only so existing config files continue to deserialize unchanged.
            timeout_secs: 900,
            max_rounds: 200,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Legacy default selection (a `[providers.*]` key). `#[serde(default)]` so a
    /// pure new-schema config selecting via `default_model` needs neither legacy
    /// field. Superseded by `default_model` (design §14.1).
    #[serde(default)]
    pub default_provider: String,
    /// Optional provider key for /goal evaluator (fast model like Haiku).
    /// Falls back to `default_provider` when not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_provider: Option<String>,
    /// Default working directory. Saved on /cd, restored on startup.
    pub default_workdir: Option<String>,
    /// Legacy flattened providers. `#[serde(default)]` so a pure new-schema
    /// config (accounts + models only, no `[providers]`) loads (design §4).
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// Provider accounts (connection + credential), keyed by account id. New
    /// schema (design §3.2); empty when only legacy `[providers.*]` are used.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provider_accounts: HashMap<String, ProviderAccountConfig>,
    /// Model profiles (selectable model + limits), keyed by selection id
    /// (recommended `<account>/<model>`). New schema (design §3.3).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub models: HashMap<String, ModelProfileConfig>,
    /// Canonical model selection (new schema, design §14.1). When set it
    /// supersedes `default_provider` for resolution (wired in Task 4). Optional
    /// so legacy configs (`default_provider` only) keep working unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Per-turn datalog settings. Missing from older configs → defaults to
    /// enabled=true, dir="$ATOMCODE_HOME/datalog" (project slug appended underneath).
    ///
    /// `skip_serializing` intentionally suppresses serde's automatic output;
    /// `save()` writes this section manually with explanatory comments and
    /// the resolved default `dir` value so users can see and edit it without
    /// having to know the field names in advance.
    #[serde(default, skip_serializing)]
    pub datalog: DatalogConfig,
    /// Task-finished notifications. Saved manually with help comments so users
    /// can discover the terminal-first strategy and platform fallbacks.
    #[serde(default, skip_serializing)]
    pub notifications: NotificationConfig,
    /// Network behavior shared by every outbound HTTP client.
    #[serde(default, skip_serializing)]
    pub network: NetworkConfig,
    /// When true (default), atomcode polls for new releases every hour
    /// while running and stages any newer version it finds. The stage is
    /// applied on the next startup (see `self_update::apply_pending_upgrade`).
    /// Set to `false` to disable auto-staging entirely; `/upgrade` still
    /// works manually. Missing from older configs → defaults to `true`.
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// Telemetry configuration. Missing from older configs → defaults to
    /// enabled=None (consent-pending), endpoint=None (use the built-in default).
    /// Uses `#[serde(default)]` because `TelemetryConfig` has its own `Default`
    /// impl that matches the no-section-present semantics.
    #[serde(default, skip_serializing)]
    pub telemetry: TelemetryConfig,
    /// LSP integration configuration.
    #[serde(default)]
    pub lsp: LspConfig,
    /// Automatically commit edited files after each agent turn completes.
    /// Only applies when working inside a git repository.
    #[serde(default)]
    pub auto_commit: bool,
    /// `task` subagent tool policy. Missing from older configs uses the defaults in
    /// [`SubAgentConfig`], including a configurable 200-round high-water mark.
    #[serde(default)]
    pub subagent: SubAgentConfig,
    /// /loop command policy. Missing from older configs → max_rounds=100.
    /// TOML section is `[loop_config]` (bare `loop` is a Rust keyword).
    #[serde(default)]
    pub loop_config: LoopConfig,
    /// `[coding]` turn-level policy. Missing from older configs → max_rounds=200.
    #[serde(default)]
    pub coding: CodingConfig,
    /// Provider key (matches a key in `Config.providers`) of a vision-language
    /// model used to preprocess images before forwarding to a non-vision main
    /// provider. When `None` or empty, image preprocessing is disabled — pasted
    /// images either go directly to a vision-capable main provider, or get
    /// degraded to `"[image attached]"` placeholder by the existing path.
    ///
    /// Example value: `"AtomGit-Qwen-Qwen3-VL-32B-Instruct"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_preprocessor_provider: Option<String>,
    /// UI / prompt language override. `None` means auto-detect from the
    /// environment (LC_ALL / LANG / system default). Persisted as the
    /// short key defined by `Locale`'s serde rename (e.g. `"zh_CN"`).
    #[serde(default)]
    pub language: Option<crate::locale::Locale>,
    /// UI rendering preferences. Currently exposes the light/dark theme
    /// switch driving the TUIX colour palette (markdown headings, code
    /// block syntax highlight, session-name pill). Missing from older
    /// configs → defaults to `dark` (legacy behaviour).
    #[serde(default)]
    pub ui: UiConfig,
    /// Plugin marketplace bootstrap + auto-update behaviour. Missing
    /// from older configs → both knobs default to `true`, matching the
    /// "ship batteries included" UX: first-startup auto-installs the
    /// official `atomcode-plugins-official` marketplace, and an in-place
    /// version upgrade silently `git pull`s every installed marketplace so
    /// plugins track the binary.
    #[serde(default)]
    pub plugin: PluginConfig,
    /// Web search backend. Missing from older configs → defaults to the
    /// `exa` provider (reachable without a VPN, returns LLM-ready result
    /// text). Set `provider = "duckduckgo"` to restore the legacy
    /// HTML-scraping backend.
    #[serde(default)]
    pub web_search: WebSearchConfig,
    /// On Ctrl-C / cancel: `true` (default) ⇒ PRESERVE the partial turn (backfill
    /// dangling tool_calls, inject an interruption marker) so the next message continues
    /// with that context. `false` ⇒ CANCEL = UNDO (the interrupted turn is rolled back).
    /// Missing from config → `true` (preserve). Set `keep_interrupted_context = false`
    /// to restore the legacy undo-on-cancel behaviour.
    #[serde(default = "default_true")]
    pub keep_interrupted_context: bool,
    /// Offline deployment switch (intranet / air-gapped). Default off = online build unchanged.
    #[serde(default)]
    pub offline_mode: offline::OfflineMode,

    /// Environment-level note appended to the OFFLINE ENVIRONMENT persona block:
    /// declares which internal mirrors/registries ARE available (e.g. npm private
    /// registry, Maven internal repo) so the model doesn't over-restrict itself.
    #[serde(default)]
    pub offline_note: Option<String>,

    /// Provider sections that failed strict validation during a *tolerant* load
    /// (see [`Self::parse_disk_content_tolerant`]). Held verbatim as raw TOML so
    /// a later write-back (`/model`, theme change, …) re-emits the user's
    /// malformed `[providers.<name>]` text unchanged instead of silently
    /// dropping it — they can repair it in place later. Never (de)serialized by
    /// serde (`skip`); populated on load and re-emitted by `serialize_for_disk`.
    #[serde(skip)]
    pub quarantined_providers: std::collections::BTreeMap<String, toml::Value>,
}

/// Web search backend configuration. Persisted as the `[web_search]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchConfig {
    /// Search backend: `"exa"` (default — MCP API at mcp.exa.ai, reachable
    /// without a VPN and returns LLM-ready result text) or `"duckduckgo"`
    /// (legacy HTML scraping of html.duckduckgo.com, blocked in some regions).
    #[serde(default = "default_search_provider")]
    pub provider: String,
    /// Optional Exa API key. Also read from the `EXA_API_KEY` env var, which
    /// takes precedence. When unset, Exa runs in its keyless tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

fn default_search_provider() -> String {
    "exa".to_string()
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: default_search_provider(),
            api_key: None,
        }
    }
}

/// Plugin / marketplace bootstrap configuration. Persisted as the
/// `[plugin]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    /// First-startup behaviour: when true (default), atomcode runs a
    /// one-time `git clone` of the official `atomcode-plugins-official`
    /// marketplace into `$ATOMCODE_HOME/plugins/marketplaces/`. A marker
    /// file (`~/.atomcode/.plugin_bootstrap_v2`) is touched after the
    /// first attempt — set or unset — so the install fires exactly
    /// once per user. A subsequent `/plugin marketplace remove` is
    /// respected; the marker stays in place and the directory is NOT
    /// recreated. To force a re-bootstrap, delete the marker.
    #[serde(default = "default_true")]
    pub auto_install_default_skills: bool,
    /// Per-startup sync: when true (default), every startup runs
    /// `git pull --ff-only` on all installed marketplaces so plugins
    /// stay in sync with the remote. Failures (no network, fast-forward
    /// conflict from local edits) are warned and ignored — never block
    /// startup.
    #[serde(default = "default_true")]
    pub auto_update_marketplaces: bool,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            auto_install_default_skills: true,
            auto_update_marketplaces: true,
        }
    }
}

fn default_auto_copy_on_select() -> bool {
    !cfg!(windows)
}

fn default_auto_copy_code_blocks() -> bool {
    // OFF by default. Auto-copying every rendered code block silently overwrote
    // the user's clipboard on each reply (issue #699 feedback), so it is opt-in.
    // Explicit `/copy` remains available regardless.
    false
}

fn default_ai_session_naming() -> bool {
    true
}

fn default_terminal_status_glyph() -> bool {
    // ON by default: a colored status dot (🟢 idle / 🟡 busy / 🔴 approval)
    // prefixed to the terminal tab title so the user can tell state without
    // switching windows. Off for terminals that render emoji as tofu boxes
    // (tmux, plain VT, some embedded IDE terminals). Read from ctx.config, so
    // /reload picks up a change.
    true
}

/// UI section of the config — currently just the theme switch driving
/// the TUIX colour palette. Persisted as a top-level `[ui]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Colour palette to use for markdown / code-block / chrome
    /// rendering. `dark` keeps the legacy palette (designed for dark
    /// terminals); `light` swaps in darker saturated variants that hit
    /// WCAG AA contrast on `#FFFFFF`. Defaults to `dark` so existing
    /// configs see no behaviour change.
    #[serde(default)]
    pub theme: UiTheme,
    /// Drag-select in the conversation auto-copies to the clipboard and
    /// shows a notice. Opt-out via `/config`. Default off on Windows
    /// (conhost QuickEdit conflict).
    #[serde(default = "default_auto_copy_on_select")]
    pub auto_copy_on_select: bool,
    /// Auto-copy a rendered code block's raw source to the clipboard when the
    /// AI finishes emitting it. OFF by default — it silently overwrote the
    /// user's clipboard on every code-block reply (issue #699 feedback). Env
    /// `ATOMCODE_AUTO_COPY` overrides this when set. Explicit `/copy` is
    /// always available regardless of this setting. Read once at startup (like
    /// `theme`), so a change takes effect on restart, not via `/reload`.
    #[serde(default = "default_auto_copy_code_blocks")]
    pub auto_copy_code_blocks: bool,
    /// AI-generate the session name from the first turn (default on). When
    /// off, the session keeps the truncation name.
    #[serde(default = "default_ai_session_naming")]
    pub ai_session_naming: bool,
    /// Prefix a colored status dot (🟢 idle / 🟡 busy / 🔴 needs-approval) to
    /// the terminal tab/window title. Default on; turn off if your terminal
    /// shows emoji as monochrome tofu boxes.
    #[serde(default = "default_terminal_status_glyph")]
    pub terminal_status_glyph: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: UiTheme::default(),
            auto_copy_on_select: default_auto_copy_on_select(),
            auto_copy_code_blocks: default_auto_copy_code_blocks(),
            ai_session_naming: default_ai_session_naming(),
            terminal_status_glyph: default_terminal_status_glyph(),
        }
    }
}

/// UI colour palette selector.
///
/// - `Auto` (default): query the terminal's background colour via
///   OSC 11 at startup and pick light or dark accordingly. Terminals
///   that don't respond (macOS Terminal.app, Windows conhost) fall
///   back to dark.
/// - `Dark` / `Light`: skip detection, use the explicit palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiTheme {
    #[default]
    Auto,
    Dark,
    Light,
}

/// Why an attached image can (or cannot) reach a model that will process it —
/// the resolution behind [`Config::can_handle_attached_images`]. Lets the paste
/// gate tell the user WHY it rejected: nothing configured (switch model / set a
/// preprocessor) vs. a preprocessor IS set but its name doesn't resolve (a
/// typo), which the old blanket "未配置" message wrongly reported as absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageAttachSupport {
    /// The active model accepts images, or `vision_preprocessor_provider`
    /// resolves — the image will be handled.
    Supported,
    /// Active model is text-only and no `vision_preprocessor_provider` is set.
    Unconfigured,
    /// `vision_preprocessor_provider` IS set but does not resolve to a real
    /// model-selection id (typo'd / removed name). Carries the offending value.
    PreprocessorUnresolvable(String),
}

impl Config {
    /// True iff attaching an image to the active turn will reach a model
    /// that can process it — either the active provider accepts images
    /// directly, or `vision_preprocessor_provider` points at a real entry
    /// in `providers` that will OCR them before forwarding. Used by the
    /// TUIX Ctrl+V paste gate to decide whether to accept the image or
    /// reject with the "switch to a vision-capable model" hint.
    pub fn can_handle_attached_images(&self) -> bool {
        matches!(self.image_attach_support(), ImageAttachSupport::Supported)
    }

    /// Resolved reason behind [`Self::can_handle_attached_images`] so the paste
    /// gate can distinguish "nothing configured" from "preprocessor configured
    /// but unresolvable" (a typo) and message accordingly.
    pub fn image_attach_support(&self) -> ImageAttachSupport {
        // Route through the single resolution boundary (§14.1) so both schemas
        // work and the active model matches what the runtime builds.
        let active_accepts = self
            .resolve_model(None)
            .map(|r| crate::util::model_name_suggests_vision(&r.model))
            .unwrap_or(false);
        if active_accepts {
            return ImageAttachSupport::Supported;
        }
        // `vision_preprocessor_provider` is a model-selection id (legacy provider
        // names still resolve via projection, §14.3): valid iff it resolves.
        match self.vision_preprocessor_provider.as_deref() {
            Some(k) if !k.is_empty() => {
                if self.resolve_model(Some(k)).is_ok() {
                    ImageAttachSupport::Supported
                } else {
                    ImageAttachSupport::PreprocessorUnresolvable(k.to_string())
                }
            }
            _ => ImageAttachSupport::Unconfigured,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: String::new(),
            evaluator_provider: None,
            default_workdir: None,
            providers: HashMap::new(),
            provider_accounts: HashMap::new(),
            models: HashMap::new(),
            default_model: None,
            datalog: Default::default(),
            notifications: Default::default(),
            network: Default::default(),
            auto_update: true,
            telemetry: Default::default(),
            lsp: Default::default(),
            auto_commit: false,
            subagent: Default::default(),
            loop_config: Default::default(),
            coding: CodingConfig::default(),
            vision_preprocessor_provider: None,
            language: None,
            ui: UiConfig::default(),
            plugin: PluginConfig::default(),
            web_search: WebSearchConfig::default(),
            keep_interrupted_context: true,
            offline_mode: offline::OfflineMode::default(),
            offline_note: None,
            quarantined_providers: std::collections::BTreeMap::new(),
        }
    }
}

impl Config {
    /// Create a `Config` with `default_provider` set and all other fields at
    /// their defaults. Useful for tests and fallback paths: [`Default`] remains
    /// the single source of truth when fields are added, while callers only
    /// specify the provider name they actually care about.
    pub fn with_default_provider(default_provider: impl Into<String>) -> Self {
        Self {
            default_provider: default_provider.into(),
            ..Default::default()
        }
    }

    /// Validate the new-schema provider accounts and model profiles. Returns a
    /// diagnostic per problem (empty ⇒ valid). Pure — does not mutate. Checks
    /// account endpoint requirements, model→account referential integrity,
    /// context/token limits, and `default_model` resolvability. Mixed-schema
    /// loading (Task 3) uses this to quarantine, rather than fail, bad entries.
    pub fn validate_provider_accounts_and_models(&self) -> Vec<String> {
        let mut diags = Vec::new();
        for (id, account) in &self.provider_accounts {
            if account.provider.trim().is_empty() {
                diags.push(format!("provider account `{id}` is missing `provider`"));
                continue;
            }
            // A preset (or the custom-compatible fallback) without a built-in
            // base URL needs one supplied on the account.
            let preset = provider_preset::preset_or_compatible(&account.provider);
            if preset.default_base_url.is_none() && account.base_url.is_none() {
                diags.push(format!(
                    "provider account `{id}` uses `{}`, which has no default endpoint; set `base_url`",
                    account.provider
                ));
            }
        }
        for (id, model) in &self.models {
            if model.model.trim().is_empty() {
                diags.push(format!("model `{id}` is missing `model`"));
            }
            if model.account.trim().is_empty() {
                diags.push(format!("model `{id}` is missing `account`"));
            } else if !self.provider_accounts.contains_key(&model.account)
                // A model may reference a legacy provider (which projects to a
                // synthetic account of the same id) — that resolves, so accept it.
                && !self.providers.contains_key(&model.account)
            {
                diags.push(format!(
                    "model `{id}` references unknown account `{}`",
                    model.account
                ));
            }
            if model.context_window == 0 {
                diags.push(format!("model `{id}` has context_window = 0"));
            }
            if model.max_tokens == Some(0) {
                diags.push(format!("model `{id}` has max_tokens = 0"));
            }
        }
        if let Some(sel) = &self.default_model {
            if !self.models.contains_key(sel) {
                diags.push(format!(
                    "default_model `{sel}` does not match any model profile"
                ));
            }
        }
        diags
    }

    /// The unified account catalog: real `provider_accounts` plus one synthetic
    /// account projected from each legacy `[providers.*]` (design §5). On an
    /// exact id collision the new-schema account wins; see
    /// [`Self::model_catalog_collisions`] for the diagnostics. Read-only — never
    /// rewrites config.
    pub fn logical_accounts(&self) -> HashMap<String, ProviderAccountConfig> {
        let mut out: HashMap<String, ProviderAccountConfig> = HashMap::new();
        for (name, p) in &self.providers {
            if is_codingplan_provider_name(name) {
                // All CodingPlan flat providers share one gateway + OAuth signer;
                // fold them into a single account per wire format. Fields are
                // uniform across the group, so the first one seen defines them.
                let id = codingplan_group_account_id(&p.provider_type);
                out.entry(id.to_string())
                    .or_insert_with(|| project_legacy_account(p));
            } else {
                out.insert(name.clone(), project_legacy_account(p));
            }
        }
        // New-schema accounts take precedence on an exact id collision.
        for (id, a) in &self.provider_accounts {
            out.insert(id.clone(), a.clone());
        }
        out
    }

    /// The unified model catalog: real `models` plus one synthetic model
    /// projected from each legacy `[providers.*]` (keyed by the legacy provider
    /// name, so `default_provider` maps to the same selection id). New-schema
    /// models win on collision.
    pub fn logical_models(&self) -> HashMap<String, ModelProfileConfig> {
        let mut out: HashMap<String, ModelProfileConfig> = HashMap::new();
        for (name, p) in &self.providers {
            // Model id stays the legacy provider name (so `default_provider`
            // resolves); only the parent account folds for CodingPlan providers.
            let account = if is_codingplan_provider_name(name) {
                codingplan_group_account_id(&p.provider_type).to_string()
            } else {
                name.clone()
            };
            out.insert(name.clone(), project_legacy_model(&account, p));
        }
        for (id, m) in &self.models {
            out.insert(id.clone(), m.clone());
        }
        out
    }

    /// Diagnostics for exact id collisions between new-schema entries and
    /// legacy provider names (the new-schema entry wins). Visible, not silent.
    pub fn model_catalog_collisions(&self) -> Vec<String> {
        let mut diags = Vec::new();
        for id in self.provider_accounts.keys() {
            if self.providers.contains_key(id) {
                diags.push(format!(
                    "provider account `{id}` collides with a legacy provider of the same name; the new-schema account wins"
                ));
            }
        }
        for id in self.models.keys() {
            if self.providers.contains_key(id) {
                diags.push(format!(
                    "model `{id}` collides with a legacy provider of the same name; the new-schema model wins"
                ));
            }
        }
        diags
    }

    /// The effective model selection id: the new `default_model` when set,
    /// otherwise the legacy `default_provider` (which projects to a synthetic
    /// model of the same id). `None` when neither is set. Bridges legacy and new
    /// selection for the single resolution boundary (Task 4).
    pub fn effective_model_selection(&self) -> Option<String> {
        self.default_model.clone().or_else(|| {
            let legacy = self.default_provider.trim();
            (!legacy.is_empty()).then(|| legacy.to_string())
        })
    }

    /// Convert a legacy `[providers.<name>]` into a new-schema account + model
    /// in place (design §5 rule 5/6). Removes the legacy entry. Pure in-memory
    /// mutation; the caller persists it through `ConfigStore` CAS. Errors if the
    /// provider is unknown or the target ids already exist in the new schema.
    pub fn upgrade_legacy_provider(&mut self, name: &str) -> Result<()> {
        let provider = self
            .providers
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("legacy provider `{name}` not found"))?;
        if self.provider_accounts.contains_key(name) || self.models.contains_key(name) {
            anyhow::bail!(
                "cannot upgrade `{name}`: a new-schema account or model already uses that id"
            );
        }
        let account = project_legacy_account(provider);
        let model = project_legacy_model(name, provider);
        // Note: manual upgrade keeps the model under its own account `name`;
        // CodingPlan grouping happens only via the read-only projection above.
        self.provider_accounts.insert(name.to_string(), account);
        self.models.insert(name.to_string(), model);
        self.providers.remove(name);
        // Keep the active selection pointing at the same model.
        if self.default_provider == name && self.default_model.is_none() {
            self.default_model = Some(name.to_string());
        }
        Ok(())
    }

    /// THE single provider/model resolution boundary (design §3.4, §10). Given a
    /// selection id (or `None` for the active [`Self::effective_model_selection`]),
    /// resolve the model profile, its account, the preset, environment API keys,
    /// and legacy projection into one flattened [`ResolvedModelConfig`] that
    /// provider construction consumes. Every consumer (footer, runtime, CLI
    /// overrides, daemon, respawn) must route through this, never re-derive from
    /// raw fields (§14.1). Errors are secret-safe: they name ids/models, never
    /// credentials.
    pub fn resolve_model(&self, selection: Option<&str>) -> Result<ResolvedModelConfig> {
        let selection_id = selection
            .map(str::to_string)
            .or_else(|| self.effective_model_selection())
            .ok_or_else(|| {
                anyhow::anyhow!("no model selected (set `default_model` or `default_provider`)")
            })?;
        let models = self.logical_models();
        let model = models
            .get(&selection_id)
            .ok_or_else(|| anyhow::anyhow!("model `{selection_id}` not found"))?;
        let accounts = self.logical_accounts();
        let account = accounts.get(&model.account).ok_or_else(|| {
            anyhow::anyhow!(
                "model `{selection_id}` references unknown account `{}`",
                model.account
            )
        })?;
        let preset = provider_preset::preset_or_compatible(&account.provider);
        let base_url = account
            .base_url
            .clone()
            .or_else(|| preset.default_base_url.map(str::to_string));
        let api_key = resolve_account_api_key(account, preset);
        Ok(ResolvedModelConfig {
            selection_id,
            account_id: model.account.clone(),
            provider_id: account.provider.clone(),
            provider_type: preset.provider_type.wire().to_string(),
            base_url,
            api_key,
            model: model.model.clone(),
            context_window: model.context_window,
            max_tokens: model.max_tokens,
            system_prompt: model.system_prompt.clone(),
            user_agent: account.user_agent.clone(),
            skip_tls_verify: account.skip_tls_verify,
            thinking_type: model.thinking_type.clone(),
            thinking_keep: model.thinking_keep.clone(),
            reasoning_history: model.reasoning_history.clone(),
            reasoning_effort: model.reasoning_effort.clone(),
            thinking_enabled: model.thinking_enabled,
            thinking_budget: model.thinking_budget,
            capable_model: model.capable_model,
            pricing: model.pricing,
        })
    }

    /// A legacy-shaped [`ProviderConfig`] view of any selection id — a legacy
    /// `[providers.*]` key OR a new-schema model id (including a folded
    /// CodingPlan model). Consumers that still key off `config.providers`
    /// (the daemon live runtime, `/think`/`/effort`) call this so a new-schema
    /// selection resolves instead of returning `None`.
    ///
    /// Legacy providers are returned verbatim (raw api_key preserved for the
    /// caller's own env expansion); new-schema selections are reconstructed via
    /// [`Self::resolve_model`].
    pub fn provider_config_for_selection(&self, selection_id: &str) -> Option<ProviderConfig> {
        // New-schema entry wins on a colliding id — same precedence as
        // `resolve_model` / `update_selection_reasoning`, so read and write agree.
        if self.models.contains_key(selection_id) {
            return self
                .resolve_model(Some(selection_id))
                .ok()
                .map(|r| r.to_provider_config());
        }
        if let Some(p) = self.providers.get(selection_id) {
            return Some(p.clone());
        }
        self.resolve_model(Some(selection_id))
            .ok()
            .map(|r| r.to_provider_config())
    }

    /// Whether a selection id resolves to any provider/model (legacy or new
    /// schema). Replaces bare `config.providers.contains_key(id)` guards that
    /// would wrongly reject a new-schema selection. Short-circuits on the raw
    /// maps first so the common case avoids materializing the folded catalog.
    pub fn selection_exists(&self, selection_id: &str) -> bool {
        self.providers.contains_key(selection_id)
            || self.models.contains_key(selection_id)
            || self.logical_models().contains_key(selection_id)
    }

    /// Apply a mutation to a selection's thinking/reasoning fields regardless of
    /// which schema stores it: prefer the new-schema `[models.*]` entry, else
    /// the legacy `[providers.*]` entry. Returns `false` when `id` names no
    /// writable target (e.g. a purely projected legacy provider is writable via
    /// `providers`; a folded-only account is not). Used by `/think`, `/effort`,
    /// the daemon reasoning-effort + thinking setters so they work on new-schema
    /// models.
    pub fn update_selection_reasoning(
        &mut self,
        id: &str,
        f: impl FnOnce(ReasoningFieldsMut<'_>),
    ) -> bool {
        if let Some(m) = self.models.get_mut(id) {
            f(ReasoningFieldsMut {
                thinking_enabled: &mut m.thinking_enabled,
                thinking_budget: &mut m.thinking_budget,
                thinking_type: &mut m.thinking_type,
                thinking_keep: &mut m.thinking_keep,
                reasoning_history: &mut m.reasoning_history,
                reasoning_effort: &mut m.reasoning_effort,
            });
            true
        } else if let Some(p) = self.providers.get_mut(id) {
            f(ReasoningFieldsMut {
                thinking_enabled: &mut p.thinking_enabled,
                thinking_budget: &mut p.thinking_budget,
                thinking_type: &mut p.thinking_type,
                thinking_keep: &mut p.thinking_keep,
                reasoning_history: &mut p.reasoning_history,
                reasoning_effort: &mut p.reasoning_effort,
            });
            true
        } else {
            false
        }
    }
}

/// Mutable borrow of the reasoning/thinking fields common to a legacy
/// `ProviderConfig` and a new-schema `ModelProfileConfig`, so callers can toggle
/// them without caring which schema stores the selection. See
/// [`Config::update_selection_reasoning`].
pub struct ReasoningFieldsMut<'a> {
    pub thinking_enabled: &'a mut Option<bool>,
    pub thinking_budget: &'a mut Option<u32>,
    pub thinking_type: &'a mut Option<String>,
    pub thinking_keep: &'a mut Option<String>,
    pub reasoning_history: &'a mut Option<String>,
    pub reasoning_effort: &'a mut Option<String>,
}

/// Resolve an account's API key with environment fallbacks, mirroring
/// [`ProviderConfig::resolved_api_key`]: an explicit `$VAR`/`${VAR}` expands, a
/// bare env-var name resolves, anything else is a literal; otherwise fall back
/// to the preset's declared env var, then the wire-type env var, then
/// `ATOMCODE_API_KEY`.
fn resolve_account_api_key(
    account: &ProviderAccountConfig,
    preset: &provider_preset::ProviderPreset,
) -> Option<String> {
    if let Some(raw) = account.api_key.as_deref() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            if trimmed.contains('$') {
                let expanded = provider::expand_env_vars(trimmed);
                if !expanded.trim().is_empty() {
                    return Some(expanded);
                }
            } else if let Ok(v) = std::env::var(trimmed) {
                if !v.trim().is_empty() {
                    return Some(v);
                }
            } else {
                return Some(trimmed.to_string());
            }
        }
    }
    let wire_env = match preset.provider_type {
        provider_preset::ProviderType::Anthropic => "ANTHROPIC_API_KEY",
        provider_preset::ProviderType::Ollama => "OLLAMA_API_KEY",
        provider_preset::ProviderType::OpenAi => "OPENAI_API_KEY",
    };
    for env in [preset.api_key_env, Some(wire_env), Some("ATOMCODE_API_KEY")]
        .into_iter()
        .flatten()
    {
        if let Ok(v) = std::env::var(env) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Map a legacy `provider_type` wire string to the preset id whose
/// [`provider_preset::ProviderType`] matches, so the projected account resolves
/// to the correct wire protocol. The legacy `base_url` is carried on the account
/// and overrides the preset default, so the choice of preset only fixes the
/// protocol, never the endpoint.
fn legacy_provider_to_preset_id(provider_type: &str) -> &'static str {
    match provider_type {
        "claude" | "anthropic" => "anthropic",
        "ollama" => "ollama",
        _ => "openai",
    }
}

/// The `[providers.*]` keys the CodingPlan login flow writes: the bare `AtomGit`
/// (single model) plus `AtomGit-<sanitized>` (multi-model). They all share one
/// gateway base_url + OAuth signer, so the projection folds them into one
/// synthetic account per wire format rather than one account each.
///
/// Single source of truth — `atomcode-codingplan` and `atomcode-tuix` delegate
/// here instead of re-implementing the prefix rule.
pub fn is_codingplan_provider_name(name: &str) -> bool {
    name == "AtomGit" || name.starts_with("AtomGit-")
}

/// The synthetic account id a legacy CodingPlan provider folds into. An account
/// carries exactly one preset (one wire format), so models are grouped by wire
/// format: openai → `AtomGit`, claude → `AtomGit-anthropic`, ollama →
/// `AtomGit-ollama`. Matches the ids the `/login` flow writes into the new
/// schema, so a re-login is a no-op transition. `pub` so `atomcode-codingplan`
/// can label the login report by account.
pub fn codingplan_group_account_id(provider_type: &str) -> &'static str {
    match legacy_provider_to_preset_id(provider_type) {
        "anthropic" => "AtomGit-anthropic",
        "ollama" => "AtomGit-ollama",
        _ => "AtomGit",
    }
}

/// Project a legacy provider into a synthetic [`ProviderAccountConfig`].
fn project_legacy_account(p: &ProviderConfig) -> ProviderAccountConfig {
    ProviderAccountConfig {
        provider: legacy_provider_to_preset_id(&p.provider_type).to_string(),
        display_name: None,
        api_key: p.api_key.clone(),
        base_url: p.base_url.clone(),
        user_agent: p.user_agent.clone(),
        skip_tls_verify: p.skip_tls_verify,
        enterprise_url: None,
        ephemeral: p.ephemeral,
    }
}

/// Project a legacy provider into a synthetic [`ModelProfileConfig`] belonging to
/// `account_id`. The model's own id (the catalog key) stays the legacy provider
/// name at the call site, so `default_provider` keeps resolving; only the parent
/// account can differ (CodingPlan providers fold into a shared account).
fn project_legacy_model(account_id: &str, p: &ProviderConfig) -> ModelProfileConfig {
    ModelProfileConfig {
        account: account_id.to_string(),
        model: p.model.clone(),
        display_name: None,
        system_prompt: p.system_prompt.clone(),
        context_window: p.context_window,
        max_tokens: p.max_tokens,
        capable_model: p.capable_model,
        thinking_type: p.thinking_type.clone(),
        thinking_keep: p.thinking_keep.clone(),
        reasoning_history: p.reasoning_history.clone(),
        reasoning_effort: p.reasoning_effort.clone(),
        thinking_enabled: p.thinking_enabled,
        thinking_budget: p.thinking_budget,
        pricing: p.pricing,
    }
}

/// Controls the per-turn markdown datalog writer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatalogConfig {
    /// When false, `DatalogWriter` becomes a no-op and no files are created.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Root directory under which datalog files are written. The per-project
    /// slug (`<basename>-<hash8>`) is always appended underneath, so two
    /// projects never collide. Accepted forms:
    /// - `None` (or omitted) → `~/.atomcode/datalog/` (default)
    /// - Absolute path        → used as-is, not affected by /cd
    /// - `~/...`              → expanded relative to home, not affected by /cd
    /// - Relative path        → resolved against working_dir, follows /cd
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

/// Controls workspace-wide outbound network behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkConfig {
    #[serde(default)]
    pub proxy: ProxyConfig,
}

/// Controls LSP (Language Server Protocol) integration.
///
/// Off by default. 5-7 atomgr datalog (build 942b615): the only `diagnostics`
/// call in a 99-turn session took 33.6s (cold rust-analyzer spin-up) and
/// returned "No diagnostics found", contributing nothing to task completion.
/// LSP is also platform/toolchain-specific (rust-analyzer, gopls, etc.) and
/// pulling those binaries unprompted violates the project's
/// tech-stack-neutrality rule. Users who want it can flip `enabled = true`
/// in their config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspConfig {
    /// Master switch for LSP diagnostics. Off by default — opt-in only.
    #[serde(default)]
    pub enabled: bool,
    /// Automatically detect and start language servers from the built-in
    /// registry. Off by default — even when `enabled = true`, users must
    /// explicitly opt in to auto-detect (or list specific `servers`) to
    /// avoid surprising the user with binary spawns.
    #[serde(default)]
    pub auto_detect: bool,
    /// Custom server configurations keyed by file extension.
    #[serde(default)]
    pub servers: std::collections::HashMap<String, crate::lsp_registry::LspServerConfig>,
    /// Time in milliseconds to wait after file sync before reading diagnostics.
    /// LSP servers need time to process notifications and publish diagnostics.
    /// Larger files or slower servers may need higher values.
    #[serde(default = "default_diagnostics_settle_delay_ms")]
    pub diagnostics_settle_delay_ms: u64,
}

fn default_diagnostics_settle_delay_ms() -> u64 {
    150
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_detect: false,
            servers: Default::default(),
            diagnostics_settle_delay_ms: default_diagnostics_settle_delay_ms(),
        }
    }
}

/// One-shot migration for users who had atomcode installed before the
/// "LSP off by default" flip (commit 5b07e2a, 2026-05-07). The setup
/// wizard at install time used `LspConfig::default()` which **at that
/// time** was `enabled=true, auto_detect=true, delay=150, servers={}`,
/// and `Config::save()` serialized those literals into
/// `~/.atomcode/config.toml`. Subsequent loads see explicit `enabled=true`
/// and ignore the new in-memory default — old installs keep spawning
/// rust-analyzer / gopls and surface init failures the user never asked
/// for.
///
/// Heuristic: if the on-disk LspConfig matches the OLD wizard-written
/// shape **byte-for-byte** (every field equals its old default), reset
/// to the new default. Any deviation (custom server, non-default delay,
/// auto_detect=false) means the user customised it intentionally —
/// leave alone.
///
/// False-positive risk: a user who manually wrote `enabled=true +
/// auto_detect=true + delay=150 + servers={}` exactly gets silently
/// reset. The shape is identical to the auto-written default, so
/// distinguishing intent is impossible without a schema-version field.
/// Probability is low; failure mode is mild (re-enable explicitly).
fn migrate_legacy_lsp_default(cfg: &mut Config) {
    let looks_auto_written = cfg.lsp.enabled
        && cfg.lsp.auto_detect
        && cfg.lsp.diagnostics_settle_delay_ms == 150
        && cfg.lsp.servers.is_empty();
    if looks_auto_written {
        cfg.lsp = LspConfig::default();
    }
}

fn default_true() -> bool {
    true
}
fn default_notification_min_duration_secs() -> u64 {
    8
}

/// Resolve the effective AI-naming flag from the env override (if any) and the
/// config value. Env values "0"/"false"/"off" (case-insensitive, trimmed) disable;
/// any other env value enables; `None` ⇒ use the config value.
fn ai_session_naming_from_parts(env_val: Option<&str>, config_val: bool) -> bool {
    match env_val {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        ),
        None => config_val,
    }
}

/// True when AI session naming should run. Env `ATOMCODE_AI_SESSION_NAMING`
/// ("0"/"false"/"off" ⇒ disabled) overrides the config value.
pub fn ai_session_naming_enabled(cfg: &Config) -> bool {
    ai_session_naming_from_parts(
        std::env::var("ATOMCODE_AI_SESSION_NAMING").ok().as_deref(),
        cfg.ui.ai_session_naming,
    )
}

/// Resolve the effective todo switch: env `ATOMCODE_TODO` (0/false/off vs 1/true/on)
/// overrides the config value; absent/empty env → config value.
pub fn todo_enabled_from_env(env: Option<&str>, cfg_value: bool) -> bool {
    match env.map(|s| s.trim().to_ascii_lowercase()) {
        Some(v) if v == "0" || v == "false" || v == "off" => false,
        Some(v) if v == "1" || v == "true" || v == "on" => true,
        _ => cfg_value,
    }
}

/// Resolve the effective `request_user_input` tool switch: DEFAULT-ON semantics.
/// Returns `false` only when `env` is `Some("")`/`"0"`/`"false"`/`"off"` (case-insensitive,
/// trimmed).  `None` (unset) or any other value → `true`.
///
/// Opt-out: set `ATOMCODE_REQUEST_USER_INPUT=0` (or `false`/`off`) to disable.
///
/// Called by `atomcode-coding`'s persona gate (`request_user_input_switch_enabled`).
///
/// NOTE: `atomcode-capabilities`' tool-registration gate contains an INTENTIONAL
/// DUPLICATE of this logic — it cannot call this helper because `atomcode-config` is not
/// a dependency of the capabilities `tools` feature layer.  If you change the logic here
/// you MUST mirror the change in
/// `atomcode-capabilities/src/tools/mod.rs` (the `request_user_input_on` block), and
/// vice versa.
pub fn request_user_input_enabled_from_env(env: Option<&str>) -> bool {
    match env.map(|s| s.trim().to_ascii_lowercase()) {
        Some(v) if v == "0" || v == "false" || v == "off" || v.is_empty() => false,
        _ => true, // default ON — unset, or any other value
    }
}

impl Default for DatalogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Pre-fill the default root so it round-trips into config.toml on
            // first save — users see exactly where logs go without having to
            // discover that "unset == ~/.atomcode/datalog". Resolver still
            // treats this string the same as `None` (project slug appended).
            dir: Some("~/.atomcode/datalog".to_string()),
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
/// `enabled` and `dir` are always emitted as real values — the default `dir`
/// (`~/.atomcode/datalog`) is shown explicitly so users see exactly where
/// logs go without having to discover that "unset == default".
fn render_datalog_section(cfg: &DatalogConfig) -> String {
    let mut out = String::new();
    out.push_str("\n# Per-turn datalog. Each turn writes a markdown summary; each LLM\n");
    out.push_str("# round writes a JSON request/response pair under `<dir>/<project>/llm/`.\n");
    out.push_str("# A per-project subdirectory is always appended under `dir` so multiple\n");
    out.push_str("# projects never share a bucket.\n");
    out.push_str("# - enabled = false        -> disable logging entirely\n");
    out.push_str("# - dir = \"~/.atomcode/datalog\" -> default (follows $HOME, ignores /cd)\n");
    out.push_str("# - dir = \"/abs/path\"      -> absolute, fixed (unaffected by /cd)\n");
    out.push_str("# - dir = \"rel/path\"       -> joined with current working_dir, follows /cd\n");
    out.push_str("[datalog]\n");
    out.push_str(&format!("enabled = {}\n", cfg.enabled));
    let dir_value = cfg.dir.as_deref().unwrap_or("~/.atomcode/datalog");
    let escaped = dir_value.replace('\\', "\\\\").replace('"', "\\\"");
    out.push_str(&format!("dir = \"{}\"\n", escaped));
    out
}

/// Re-emit provider tables that failed strict validation on load, verbatim,
/// so a write-back preserves the user's malformed `[providers.<name>]` text
/// rather than silently dropping it (see [`Config::quarantined_providers`]).
/// Rendered through the toml serializer so key quoting round-trips exactly to
/// what the loader reads back.
///
/// All entries are serialized in a SINGLE pass. Emitting them one at a time
/// produced a separate `[providers]` header for every *non-table* value (e.g.
/// `providers.Foo = "bar"`), and two such headers are a duplicate-table TOML
/// error that would corrupt the file on write-back. Serializing the whole map
/// at once groups any scalar entries under one `[providers]` header while each
/// table entry still gets its own `[providers.<name>]`.
fn render_quarantined_providers(
    quarantined: &std::collections::BTreeMap<String, toml::Value>,
) -> String {
    if quarantined.is_empty() {
        return String::new();
    }
    let mut providers = toml::map::Map::new();
    for (name, value) in quarantined {
        providers.insert(name.clone(), value.clone());
    }
    let mut wrapper = toml::map::Map::new();
    wrapper.insert("providers".to_string(), toml::Value::Table(providers));
    // Values we deserialized from disk always re-serialize; if the batch somehow
    // can't, drop it rather than write a corrupt file.
    let Ok(section) = toml::to_string_pretty(&toml::Value::Table(wrapper)) else {
        return String::new();
    };
    let mut out = String::new();
    out.push_str("\n# The provider section(s) below failed validation on load and were\n");
    out.push_str("# kept as-is so nothing you wrote is lost. Fix or delete them; until\n");
    out.push_str("# then they are ignored (see the startup warning for the reason).\n\n");
    out.push_str(section.trim_end());
    out.push('\n');
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

fn render_network_section(cfg: &NetworkConfig) -> String {
    let mut out = String::new();
    out.push_str("\n# Network proxy policy shared by all outbound HTTP clients.\n");
    out.push_str("# Modes:\n");
    out.push_str(
        "# - follow_system  -> follow the launch environment / system proxy state (default)\n",
    );
    out.push_str(
        "# - default_proxy  -> pin the proxy values below and reuse them on future launches\n",
    );
    out.push_str("# - no_proxy       -> disable proxy resolution entirely\n");
    out.push_str("[network.proxy]\n");
    out.push_str(&format!("mode = \"{}\"\n", cfg.proxy.mode.label()));
    match &cfg.proxy.http {
        Some(v) => out.push_str(&format!("http = \"{}\"\n", escape_toml(v))),
        None => out.push_str("# http = \"http://127.0.0.1:7890\"\n"),
    }
    match &cfg.proxy.https {
        Some(v) => out.push_str(&format!("https = \"{}\"\n", escape_toml(v))),
        None => out.push_str("# https = \"http://127.0.0.1:7890\"\n"),
    }
    match &cfg.proxy.all {
        Some(v) => out.push_str(&format!("all = \"{}\"\n", escape_toml(v))),
        None => out.push_str("# all = \"socks5://127.0.0.1:7890\"\n"),
    }
    match &cfg.proxy.no_proxy {
        Some(v) => out.push_str(&format!("no_proxy = \"{}\"\n", escape_toml(v))),
        None => out.push_str("# no_proxy = \"localhost,127.0.0.1\"\n"),
    }
    out
}

fn render_telemetry_section(cfg: &TelemetryConfig) -> String {
    if cfg.enabled.is_none() && cfg.endpoint.is_none() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("\n# Anonymous telemetry. Omit `enabled` for the default enabled behavior.\n");
    out.push_str("# Set `enabled = false` to opt out persistently.\n");
    out.push_str("[telemetry]\n");
    if let Some(enabled) = cfg.enabled {
        out.push_str(&format!("enabled = {}\n", enabled));
    }
    if let Some(endpoint) = cfg.endpoint.as_deref() {
        out.push_str(&format!("endpoint = \"{}\"\n", escape_toml(endpoint)));
    }
    out
}

fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render a documentation comment about the layered instruction file system.
/// Always emitted (even on first save) so users discover the feature.
fn render_instructions_section() -> String {
    let mut out = String::new();
    out.push_str("\n# Project instructions — customize AI behavior via Markdown files.\n");
    out.push_str("# AtomCode loads instructions from three levels (low → high priority):\n");
    out.push_str("#\n");
    out.push_str("#   1. ~/.atomcode/ATOMCODE.md           (global — your personal defaults)\n");
    out.push_str(
        "#   2. <project>/.atomcode.md            (project — team-shared, commit to git)\n",
    );
    out.push_str("#      or <project>/ATOMCODE.md\n");
    out.push_str("#      or <project>/AGENTS.md           (AGENTS.md open standard)\n");
    out.push_str("#      or <project>/CLAUDE.md / claude.md (Claude Code compat)\n");
    out.push_str(
        "#   3. <project>/.atomcode.user.md       (user — personal per-project, .gitignore)\n",
    );
    out.push_str("#\n");
    out.push_str("# Higher priority files appear later in the prompt (recency effect).\n");
    out.push_str(
        "# Use /status to see which files are loaded. Use /init to generate a template.\n",
    );
    out.push_str("#\n");
    out.push_str("# Example ~/.atomcode/ATOMCODE.md:\n");
    out.push_str("#   ## Global Preferences\n");
    out.push_str("#   - Reply in Chinese\n");
    out.push_str("#   - Don't add AI co-author tags to commits\n");
    out.push_str("#\n");
    out.push_str("# Example <project>/.atomcode.md:\n");
    out.push_str("#   ## Project Rules\n");
    out.push_str("#   - This is a Rust workspace with 5 crates\n");
    out.push_str("#   - Use anyhow::Result for error handling\n");
    out.push_str("#   - All public APIs must have doc comments\n");
    out
}

fn render_hooks_json_section() -> String {
    let mut out = String::new();
    out.push_str("\n# Lifecycle hooks — configure in separate JSON files:\n");
    out.push_str("#   ~/.atomcode/hooks.json       (global hooks)\n");
    out.push_str("#   <project>/.hooks.json         (project hooks, override global by name)\n");
    out.push_str("#\n");
    out.push_str("# Example hooks.json:\n");
    out.push_str("#   {\n");
    out.push_str("#     \"hooks\": {\n");
    out.push_str("#       \"audit-all\": {\n");
    out.push_str("#         \"event\": \"pre_tool_use\",\n");
    out.push_str("#         \"command\": \"echo \\\"$(date) $ATOMCODE_TOOL_NAME\\\" >> ~/.atomcode/audit.log\"\n");
    out.push_str("#       },\n");
    out.push_str("#       \"block-rm\": {\n");
    out.push_str("#         \"event\": \"pre_tool_use\",\n");
    out.push_str("#         \"matcher\": \"bash\",\n");
    out.push_str("#         \"command\": \"your-safety-check.sh\",\n");
    out.push_str("#         \"timeout_ms\": 5000\n");
    out.push_str("#       }\n");
    out.push_str("#     }\n");
    out.push_str("#   }\n");
    out.push_str("#\n");
    out.push_str("# Events: pre_tool_use, post_tool_use, session_start, session_end\n");
    out.push_str("# Env vars: ATOMCODE_HOOK_EVENT, ATOMCODE_TOOL_NAME, ATOMCODE_HOOK_CONTEXT\n");
    out.push_str("# PreToolUse stdout: {\"action\":\"allow\"} or {\"action\":\"block\",\"reason\":\"...\"}\n");
    out
}

impl Config {
    /// Context window of the active selection, resolved through the single
    /// [`Self::resolve_model`] boundary (§14.1) so the displayed window can never
    /// diverge from what the runtime builds. Falls back to 128_000 when nothing
    /// resolves. For a legacy config this equals the old
    /// `providers[default_provider].context_window` lookup (the legacy provider
    /// projects to a model of the same id), so behavior is unchanged.
    pub fn default_context_window(&self) -> usize {
        self.resolve_model(None)
            .map(|r| r.context_window)
            .unwrap_or(128_000)
    }

    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with_diagnostics(path).map(|(config, _warnings)| config)
    }

    /// Strictly validate every section in a config file.
    ///
    /// Use this at persistence and seed-import boundaries where accepting a
    /// partially valid document would incorrectly bless malformed input.
    pub fn load_strict(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config: {}", path.display()))?;
        Self::parse_disk_content(&content, path)
    }

    /// Load a user-edited config while isolating invalid provider sections.
    ///
    /// The strict [`Self::load_strict`] path remains the validation boundary
    /// for seeds and writes. Interactive startup uses the diagnostics so one
    /// malformed `[providers.<name>]` table cannot silently disappear.
    pub fn load_with_diagnostics(path: &Path) -> Result<(Self, Vec<String>)> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config: {}", path.display()))?;
        Self::parse_disk_content_tolerant(&content, path)
    }

    pub(crate) fn parse_disk_content(content: &str, path: &Path) -> Result<Self> {
        let mut config: Config = toml::from_str(content)
            .with_context(|| format!("Failed to parse config: {}", path.display()))?;
        migrate_legacy_lsp_default(&mut config);
        Ok(config)
    }

    pub(crate) fn parse_disk_content_tolerant(
        content: &str,
        path: &Path,
    ) -> Result<(Self, Vec<String>)> {
        let mut document: toml::Value = toml::from_str(content)
            .with_context(|| format!("Failed to parse config: {}", path.display()))?;
        let mut warnings = Vec::new();
        let mut quarantined: std::collections::BTreeMap<String, toml::Value> =
            std::collections::BTreeMap::new();

        if let Some(providers) = document
            .get_mut("providers")
            .and_then(toml::Value::as_table_mut)
        {
            let names: Vec<String> = providers.keys().cloned().collect();
            for name in names {
                let Some(value) = providers.get(&name).cloned() else {
                    continue;
                };
                // Validate against a clone so the original raw value survives to
                // be quarantined verbatim on the error path.
                if let Err(error) = value.clone().try_into::<ProviderConfig>() {
                    providers.remove(&name);
                    warnings.push(format!("[providers.{name}] was ignored: {error}"));
                    quarantined.insert(name, value);
                }
            }
        }

        let mut config: Config = document
            .try_into()
            .with_context(|| format!("Failed to parse config: {}", path.display()))?;
        config.quarantined_providers = quarantined;
        migrate_legacy_lsp_default(&mut config);
        if config
            .quarantined_providers
            .contains_key(&config.default_provider)
        {
            let unavailable = config.default_provider.clone();
            if let Some(fallback) = config.providers.keys().min().cloned() {
                config.default_provider = fallback.clone();
                warnings.push(format!(
                    "default_provider \"{unavailable}\" was unavailable; using \"{fallback}\""
                ));
            } else {
                warnings.push(format!(
                    "default_provider \"{unavailable}\" was unavailable; no valid providers remain"
                ));
            }
        }
        Ok((config, warnings))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::store::ConfigStore::new(path).replace(self)?;
        Ok(())
    }

    pub(crate) fn serialize_for_disk(&self, disk: Option<&Config>) -> Result<String> {
        // Filter out ephemeral providers (e.g. OAuth /login) — they live in memory only.
        let mut persistent = self.clone();
        persistent.providers.retain(|_, v| !v.ephemeral);
        // Same for ephemeral provider accounts (their api_key is runtime-only),
        // and drop any model profile that would be orphaned by that removal so
        // the saved file never references a stripped account.
        let ephemeral_accounts: std::collections::HashSet<String> = self
            .provider_accounts
            .iter()
            .filter(|(_, a)| a.ephemeral)
            .map(|(k, _)| k.clone())
            .collect();
        persistent.provider_accounts.retain(|_, a| !a.ephemeral);
        persistent
            .models
            .retain(|_, m| !ephemeral_accounts.contains(&m.account));
        // If `default_model` pointed at a now-stripped ephemeral model, don't
        // persist a dangling selection — restore the disk value if we have one,
        // else clear it so `resolve_model(None)` falls back cleanly.
        if let Some(sel) = persistent.default_model.clone() {
            if !persistent.models.contains_key(&sel) && !persistent.providers.contains_key(&sel) {
                persistent.default_model = disk.and_then(|d| d.default_model.clone());
            }
        }
        // If default_provider is ephemeral, don't change the saved default
        if !self
            .providers
            .get(&self.default_provider)
            .map(|p| !p.ephemeral)
            .unwrap_or(true)
        {
            // Restore original default from disk if possible
            if let Some(disk) = disk {
                persistent.default_provider = disk.default_provider.clone();
            }
        }
        let mut content = toml::to_string_pretty(&persistent)?;
        content.push_str(&render_datalog_section(&self.datalog));
        content.push_str(&render_notifications_section(&self.notifications));
        content.push_str(&render_network_section(&self.network));
        content.push_str(&render_telemetry_section(&self.telemetry));
        content.push_str(&render_instructions_section());
        content.push_str(&render_hooks_json_section());
        content.push_str(&render_quarantined_providers(&self.quarantined_providers));
        Ok(content)
    }

    /// The active provider resolved to a legacy-shaped [`ProviderConfig`], via
    /// the unified catalog so a new-schema / folded-CodingPlan selection resolves
    /// (its id no longer lives in `config.providers`). Falls back to the first
    /// catalog model when the selection is empty or dangling, so the TUI still
    /// boots and the user can self-correct via `/provider`.
    pub fn active_provider(&self, override_name: Option<&str>) -> Result<ProviderConfig> {
        let selection = override_name
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.effective_model_selection());
        let first_catalog = || {
            let mut ids: Vec<String> = self.logical_models().into_keys().collect();
            ids.sort();
            ids.into_iter().next()
        };
        let name = selection
            .filter(|s| self.selection_exists(s))
            .or_else(first_catalog)
            .ok_or_else(|| anyhow::anyhow!("No providers configured — run /login or /provider"))?;
        self.provider_config_for_selection(&name)
            .ok_or_else(|| anyhow::anyhow!("No providers configured — run /login or /provider"))
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
            crate::util::real_home_dir(),
        )
    }

    pub fn default_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    /// FIRST-RUN ONLY config seed for offline / managed deploys (e.g. a government
    /// intranet that ships a bundled `atomcode-default-config.toml` next to the
    /// binary and points `--seed-config` / `ATOMCODE_SEED_CONFIG` at it).
    ///
    /// If `config_path` does NOT yet exist and `seed_source` is a readable, parseable
    /// config, copy it into place so the very first launch is already configured (no
    /// per-machine setup). The user then owns that writable copy — this NEVER
    /// overwrites an existing config, so the launcher can pass the flag unconditionally.
    ///
    /// Every failure mode is non-fatal and returned (not panicked / logged here) so the
    /// caller can warn and fall back to normal onboarding — a bad seed must never block
    /// startup. The raw file is copied verbatim (comments/formatting preserved), only
    /// after `Config::load` confirms it parses.
    pub fn seed_user_config(config_path: &Path, seed_source: Option<&Path>) -> SeedOutcome {
        if config_path.exists() {
            return SeedOutcome::AlreadyConfigured;
        }
        let Some(src) = seed_source else {
            return SeedOutcome::NoSource;
        };
        // Validate by loading (read + toml parse + migrations) before adopting, so a
        // malformed seed can't wedge the user into a broken config.
        let seed = match Config::load_strict(src) {
            Ok(c) => c,
            Err(e) => return SeedOutcome::Invalid(e.to_string()),
        };
        // Parse-valid is not enough: a seed whose `default_provider` is empty or
        // doesn't name a real `[providers.*]` entry (an easy IT typo) would copy in
        // fine, then launch the user into a config that EXISTS but has no working
        // provider — and because it exists, onboarding won't fire, so there's no
        // recovery hint. Reject it here so we fall back to onboarding instead.
        if seed.default_provider.is_empty() {
            return SeedOutcome::Invalid("seed config has no default_provider set".to_string());
        }
        if !seed.providers.contains_key(&seed.default_provider) {
            return SeedOutcome::Invalid(format!(
                "seed config default_provider \"{}\" does not match any [providers.*] entry",
                seed.default_provider
            ));
        }
        if let Some(parent) = config_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return SeedOutcome::IoError(e.to_string());
            }
        }
        if let Err(e) = std::fs::copy(src, config_path) {
            return SeedOutcome::IoError(e.to_string());
        }
        SeedOutcome::Seeded
    }
}

/// Result of [`Config::seed_user_config`]. Only `Seeded` changed anything on disk.
#[derive(Debug, PartialEq, Eq)]
pub enum SeedOutcome {
    /// Copied the seed into `config_path`.
    Seeded,
    /// The user already had a config — left untouched (the common steady-state case).
    AlreadyConfigured,
    /// No `--seed-config` / `ATOMCODE_SEED_CONFIG` provided (the default for normal builds).
    NoSource,
    /// Seed file was unreadable or not a valid config — skipped, keep onboarding.
    Invalid(String),
    /// Filesystem error creating/writing the target — skipped, keep onboarding.
    IoError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: the provider-type key in TOML is `type` (ProviderConfig uses
    // #[serde(rename = "type")]), NOT `provider_type`.
    const SEED_TOML: &str = "default_provider = \"glm-internal\"\n\
        [providers.glm-internal]\n\
        type = \"openai\"\n\
        base_url = \"http://gw.internal/v1\"\n\
        model = \"glm-5.2\"\n\
        api_key = \"internal\"\n";

    #[test]
    fn seed_copies_when_no_user_config() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("atomcode-default-config.toml");
        std::fs::write(&seed, SEED_TOML).unwrap();
        let target = dir.path().join("home/.atomcode/config.toml");

        let outcome = Config::seed_user_config(&target, Some(&seed));
        assert_eq!(outcome, SeedOutcome::Seeded);
        assert!(target.exists(), "seed must create the user config");
        // Copied verbatim + parses to the internal provider default.
        let loaded = Config::load(&target).unwrap();
        assert_eq!(loaded.default_provider, "glm-internal");
    }

    #[test]
    fn seed_never_overwrites_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed.toml");
        std::fs::write(&seed, SEED_TOML).unwrap();
        let target = dir.path().join("config.toml");
        std::fs::write(&target, "default_provider = \"mine\"\n").unwrap();

        let outcome = Config::seed_user_config(&target, Some(&seed));
        assert_eq!(outcome, SeedOutcome::AlreadyConfigured);
        // User's file is untouched.
        assert!(std::fs::read_to_string(&target).unwrap().contains("mine"));
    }

    #[test]
    fn seed_no_source_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        assert_eq!(
            Config::seed_user_config(&target, None),
            SeedOutcome::NoSource
        );
        assert!(!target.exists());
    }

    #[test]
    fn seed_rejects_malformed_source() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("bad.toml");
        std::fs::write(&seed, "this = is = not valid toml =\n").unwrap();
        let target = dir.path().join("config.toml");

        let outcome = Config::seed_user_config(&target, Some(&seed));
        assert!(matches!(outcome, SeedOutcome::Invalid(_)));
        assert!(!target.exists(), "a bad seed must not create a config");
    }

    #[test]
    fn tolerant_load_skips_only_the_invalid_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
default_provider = "AtomGit"
auto_update = false

[providers.AtomGit]
type = "openai"
base_url = "https://example.com/v1"
api_key = "valid"
model = "glm-5.2"

[providers.MyDeepSeek]
base_url = "https://api.siliconflow.cn/v1"
api_key = "also-secret"
model = "deepseek-ai/DeepSeek-V4-Pro"
capable_model = 1
"#,
        )
        .unwrap();

        assert!(
            Config::load_strict(&path).is_err(),
            "strict validation must continue rejecting the malformed file"
        );

        let default_load = Config::load(&path).unwrap();
        assert!(default_load.providers.contains_key("AtomGit"));
        assert!(!default_load.providers.contains_key("MyDeepSeek"));

        let (config, warnings) = Config::load_with_diagnostics(&path).unwrap();
        assert_eq!(config.default_provider, "AtomGit");
        assert!(!config.auto_update);
        assert_eq!(config.providers.len(), 1);
        assert!(config.providers.contains_key("AtomGit"));
        assert!(!config.providers.contains_key("MyDeepSeek"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("[providers.MyDeepSeek]"));
        assert!(warnings[0].contains("missing field"));
        assert!(
            !warnings[0].contains("also-secret"),
            "diagnostics must not expose provider credentials"
        );

        // The invalid section is quarantined verbatim (not discarded) so a
        // write-back can preserve it.
        assert!(config.quarantined_providers.contains_key("MyDeepSeek"));
        assert!(!config.quarantined_providers.contains_key("AtomGit"));
    }

    #[test]
    fn serialize_round_trip_preserves_a_quarantined_provider() {
        let source = r#"
default_provider = "Valid"

[providers.Valid]
type = "openai"
model = "working-model"

[providers.Broken]
model = "missing-type"
api_key = "keep-me-secret"
"#;
        let (config, warnings) =
            Config::parse_disk_content_tolerant(source, Path::new("x")).unwrap();
        assert_eq!(config.providers.len(), 1);
        assert_eq!(warnings.len(), 1);

        // Serialize (as a write-back would) then reload: the Broken section and
        // its fields survive, and it is still isolated from the typed view.
        let rendered = config.serialize_for_disk(None).unwrap();
        assert!(rendered.contains("[providers.Broken]"));
        assert!(rendered.contains("missing-type"));
        assert!(rendered.contains("keep-me-secret"));

        let (reloaded, rewarnings) =
            Config::parse_disk_content_tolerant(&rendered, Path::new("x")).unwrap();
        assert_eq!(reloaded.default_provider, "Valid");
        assert!(reloaded.providers.contains_key("Valid"));
        assert!(!reloaded.providers.contains_key("Broken"));
        assert!(reloaded.quarantined_providers.contains_key("Broken"));
        assert_eq!(rewarnings.len(), 1);
    }

    #[test]
    fn serialize_round_trip_survives_multiple_non_table_providers() {
        // Two providers written as inline scalars (`providers.Foo = "..."`) both
        // fail validation. Emitting them one-per-`to_string_pretty` call produced
        // two `[providers]` headers — a duplicate-table TOML error that corrupted
        // the file on write-back. A single serialize pass must round-trip cleanly.
        let source = r#"
default_provider = "V"

[providers.V]
type = "openai"
model = "m"

[providers]
Foo = "one"
Baz = "two"
"#;
        let (config, warnings) =
            Config::parse_disk_content_tolerant(source, Path::new("x")).unwrap();
        assert_eq!(config.providers.len(), 1);
        assert_eq!(warnings.len(), 2);

        let rendered = config.serialize_for_disk(None).unwrap();
        // The critical assertion: the written file must re-parse (not corrupt).
        let (reloaded, _) = Config::parse_disk_content_tolerant(&rendered, Path::new("x"))
            .expect("write-back must produce valid TOML for multiple non-table providers");
        assert!(reloaded.providers.contains_key("V"));
        assert!(reloaded.quarantined_providers.contains_key("Foo"));
        assert!(reloaded.quarantined_providers.contains_key("Baz"));
    }

    #[test]
    fn tolerant_load_replaces_a_skipped_default_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
default_provider = "Broken"

[providers.Valid]
type = "openai"
model = "working-model"

[providers.Broken]
model = "missing-type"
"#,
        )
        .unwrap();

        let (config, warnings) = Config::load_with_diagnostics(&path).unwrap();
        assert_eq!(config.default_provider, "Valid");
        assert_eq!(config.providers.len(), 1);
        assert!(warnings.iter().any(
            |warning| warning == "default_provider \"Broken\" was unavailable; using \"Valid\""
        ));
    }

    #[test]
    fn seed_rejects_unresolvable_default_provider() {
        // Parses fine, but default_provider names no [providers.*] entry (IT typo).
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("typo.toml");
        std::fs::write(
            &seed,
            "default_provider = \"glm-internel\"\n\
             [providers.glm-internal]\n\
             type = \"openai\"\n\
             model = \"glm-5.2\"\n",
        )
        .unwrap();
        let target = dir.path().join("config.toml");

        let outcome = Config::seed_user_config(&target, Some(&seed));
        assert!(
            matches!(outcome, SeedOutcome::Invalid(_)),
            "unresolvable default_provider must be rejected, got {outcome:?}"
        );
        assert!(!target.exists(), "a broken seed must not create a config");
    }

    #[test]
    fn seed_rejects_empty_default_provider() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("noprov.toml");
        // Parses fine (explicit empty string), but names no provider → reject.
        std::fs::write(&seed, "default_provider = \"\"\n").unwrap();
        let target = dir.path().join("config.toml");

        let outcome = Config::seed_user_config(&target, Some(&seed));
        assert!(matches!(outcome, SeedOutcome::Invalid(_)));
        assert!(!target.exists());
    }

    #[test]
    fn seed_missing_source_file_is_invalid_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        let outcome = Config::seed_user_config(&target, Some(&dir.path().join("nope.toml")));
        assert!(matches!(outcome, SeedOutcome::Invalid(_)));
        assert!(!target.exists());
    }

    /// LSP must default to disabled. 5-7 atomgr datalog (build 942b615):
    /// the only `diagnostics` call in 99 turns took 33.6s for a "No
    /// diagnostics found" reply. Spinning up rust-analyzer / gopls /
    /// pyright unprompted also conflicts with the framework's
    /// tech-stack-neutrality stance. Users must opt in explicitly.
    #[test]
    fn lsp_config_defaults_to_disabled_opt_in() {
        let cfg = LspConfig::default();
        assert!(!cfg.enabled, "LSP enabled must default to false");
        assert!(
            !cfg.auto_detect,
            "LSP auto_detect must default to false even if enabled flips on"
        );
    }

    #[test]
    fn auto_copy_on_select_defaults_per_platform() {
        let ui = UiConfig::default();
        assert_eq!(ui.auto_copy_on_select, !cfg!(windows));
    }

    #[test]
    fn auto_copy_code_blocks_defaults_off() {
        // Opt-in only: the code-block auto-copy must not hijack the clipboard
        // unless the user explicitly turns it on (issue #699 feedback).
        assert!(!UiConfig::default().auto_copy_code_blocks);
        // A config that omits the key (older configs) also lands OFF.
        let ui: UiConfig = toml::from_str("theme = \"dark\"").unwrap();
        assert!(!ui.auto_copy_code_blocks, "missing key → default off");
    }

    #[test]
    fn terminal_status_glyph_defaults_on() {
        // Default-on: fresh config and a config missing the key both enable it.
        assert!(UiConfig::default().terminal_status_glyph);
        let ui: UiConfig = toml::from_str("").unwrap();
        assert!(ui.terminal_status_glyph, "missing key → default on");
    }

    #[test]
    fn ai_session_naming_defaults_true() {
        let ui = UiConfig::default();
        assert!(ui.ai_session_naming);
    }

    #[test]
    fn ai_session_naming_enabled_respects_config() {
        let cfg = Config::default();
        assert!(ai_session_naming_enabled(&cfg));
    }

    #[test]
    fn ai_naming_env_disables_case_insensitively() {
        for v in ["0", "false", "off", "FALSE", "  Off  ", "OFF"] {
            assert!(
                !super::ai_session_naming_from_parts(Some(v), true),
                "{v} should disable"
            );
        }
    }

    #[test]
    fn ai_naming_env_enables_for_other_values() {
        for v in ["1", "true", "on", "yes", ""] {
            assert!(
                super::ai_session_naming_from_parts(Some(v), true),
                "{v} should enable"
            );
        }
        // empty string trims to empty, not a disable token → enable
    }

    #[test]
    fn ai_naming_falls_through_to_config_when_env_unset() {
        assert!(super::ai_session_naming_from_parts(None, true));
        assert!(!super::ai_session_naming_from_parts(None, false));
    }

    #[test]
    fn ui_todo_env_off_overrides() {
        assert!(!super::todo_enabled_from_env(Some("0"), true));
        assert!(super::todo_enabled_from_env(Some("1"), false));
        assert!(super::todo_enabled_from_env(None, true)); // 无 env → 用 config 值
    }

    #[test]
    fn request_user_input_enabled_default_on() {
        // None (unset) → true (default-ON)
        assert!(super::request_user_input_enabled_from_env(None));
        // Explicit opt-out values → false
        assert!(!super::request_user_input_enabled_from_env(Some("")));
        assert!(!super::request_user_input_enabled_from_env(Some("0")));
        assert!(!super::request_user_input_enabled_from_env(Some("false")));
        assert!(!super::request_user_input_enabled_from_env(Some("FALSE")));
        assert!(!super::request_user_input_enabled_from_env(Some("off")));
        assert!(!super::request_user_input_enabled_from_env(Some("OFF")));
        assert!(!super::request_user_input_enabled_from_env(Some("  off  ")));
        // Any other value (truthy) → true
        assert!(super::request_user_input_enabled_from_env(Some("1")));
        assert!(super::request_user_input_enabled_from_env(Some("true")));
        assert!(super::request_user_input_enabled_from_env(Some("yes")));
        assert!(super::request_user_input_enabled_from_env(Some("on")));
    }

    /// Migration: on-disk config that looks like it was auto-written by
    /// the OLD setup wizard (enabled=true + auto_detect=true + delay=150
    /// + no custom servers) must be silently reset to disabled. Without
    /// this, users installed before commit 5b07e2a keep spawning
    /// rust-analyzer / gopls every startup despite the new default.
    #[test]
    fn migrate_resets_auto_written_lsp_to_disabled() {
        let mut cfg = blank_config_with_lsp(LspConfig {
            enabled: true,
            auto_detect: true,
            servers: Default::default(),
            diagnostics_settle_delay_ms: 150,
        });
        migrate_legacy_lsp_default(&mut cfg);
        assert!(
            !cfg.lsp.enabled,
            "auto-written shape must reset to disabled"
        );
        assert!(!cfg.lsp.auto_detect);
    }

    /// User who deliberately customised LSP (e.g. added a custom server
    /// or tuned the settle delay) must NOT be reset. Migration only fires
    /// for byte-perfect old-default shape.
    #[test]
    fn migrate_keeps_user_customised_lsp_intact() {
        // Case 1: custom server registered.
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "rs".to_string(),
            crate::lsp_registry::LspServerConfig {
                command: "my-custom-rust-ls".to_string(),
                args: vec![],
                root_markers: vec![],
            },
        );
        let mut cfg = blank_config_with_lsp(LspConfig {
            enabled: true,
            auto_detect: true,
            servers,
            diagnostics_settle_delay_ms: 150,
        });
        migrate_legacy_lsp_default(&mut cfg);
        assert!(cfg.lsp.enabled, "custom servers means user opt-in; keep");

        // Case 2: tuned settle delay.
        let mut cfg2 = blank_config_with_lsp(LspConfig {
            enabled: true,
            auto_detect: true,
            servers: Default::default(),
            diagnostics_settle_delay_ms: 500,
        });
        migrate_legacy_lsp_default(&mut cfg2);
        assert!(cfg2.lsp.enabled, "non-default delay means user tuned; keep");

        // Case 3: auto_detect=false but enabled=true (explicit narrow
        // setup with `servers` listed) — already deviates, keep.
        let mut cfg3 = blank_config_with_lsp(LspConfig {
            enabled: true,
            auto_detect: false,
            servers: Default::default(),
            diagnostics_settle_delay_ms: 150,
        });
        migrate_legacy_lsp_default(&mut cfg3);
        assert!(
            cfg3.lsp.enabled,
            "auto_detect=false means user picked manual; keep"
        );
    }

    /// Already-disabled config: migration must be a no-op (don't flip
    /// disabled → re-disabled, but more importantly don't trigger any
    /// surprise side effects).
    #[test]
    fn migrate_noop_on_already_disabled() {
        let mut cfg = blank_config_with_lsp(LspConfig::default());
        migrate_legacy_lsp_default(&mut cfg);
        assert!(!cfg.lsp.enabled);
        assert!(!cfg.lsp.auto_detect);
    }

    fn blank_config_with_lsp(lsp: LspConfig) -> Config {
        Config {
            lsp,
            ..Config::with_default_provider("x")
        }
    }

    /// Empty/missing `[lsp]` section in user TOML must produce the
    /// disabled default — not silently flip back to enabled via a
    /// stray `default = "default_true"` serde attribute.
    #[test]
    fn lsp_section_omitted_in_toml_yields_disabled() {
        let toml_str = r#"
            default_provider = "claude"

            [providers.claude]
            type = "claude"
            api_key = "sk-ant-test"
            model = "claude-opus-4-6"
        "#;
        let cfg: Config = toml::from_str(toml_str).expect("config parses");
        assert!(!cfg.lsp.enabled, "missing [lsp] must keep LSP off");
        assert!(!cfg.lsp.auto_detect);
    }

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
    fn render_datalog_section_default_emits_active_dir() {
        let rendered = render_datalog_section(&DatalogConfig::default());
        assert!(rendered.contains("[datalog]"));
        assert!(rendered.contains("enabled = true"));
        assert!(
            rendered.contains("\ndir = \"~/.atomcode/datalog\"\n"),
            "default must emit the resolved dir as a real, uncommented value: {}",
            rendered
        );
    }

    #[test]
    fn render_datalog_section_unset_dir_still_shows_default() {
        // Belt-and-suspenders: even if some caller hands us a Config where
        // `dir` somehow ended up None (older config file, manual deserialize),
        // render still emits the default value rather than omitting the line.
        let cfg = DatalogConfig {
            enabled: true,
            dir: None,
        };
        let rendered = render_datalog_section(&cfg);
        assert!(rendered.contains("\ndir = \"~/.atomcode/datalog\"\n"));
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
            evaluator_provider: None,
            default_workdir: None,
            providers: HashMap::new(),
            provider_accounts: HashMap::new(),
            models: HashMap::new(),
            default_model: None,
            datalog: DatalogConfig {
                enabled: false,
                dir: Some("/var/log/ac".to_string()),
            },
            notifications: NotificationConfig::default(),
            network: NetworkConfig::default(),
            auto_update: true,
            telemetry: Default::default(),
            lsp: Default::default(),
            auto_commit: false,
            subagent: Default::default(),
            loop_config: Default::default(),
            coding: CodingConfig::default(),
            vision_preprocessor_provider: None,
            language: None,
            ui: Default::default(),
            plugin: Default::default(),
            web_search: Default::default(),
            keep_interrupted_context: false,
            offline_mode: Default::default(),
            offline_note: None,
            quarantined_providers: std::collections::BTreeMap::new(),
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
                reasoning_effort: None,
                thinking_enabled: None,
                thinking_budget: None,
                skip_tls_verify: false,
                ephemeral: false,
                capable_model: None,
                pricing: None,
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
        assert_eq!(
            reloaded.network.proxy.mode,
            crate::proxy::ProxyMode::FollowSystem
        );
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
    fn render_network_section_emits_proxy_mode() {
        let rendered = render_network_section(&NetworkConfig::default());
        assert!(rendered.contains("[network.proxy]"));
        assert!(rendered.contains("mode = \"follow_system\""));
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
    #[test]
    fn active_provider_falls_back_when_default_points_to_deleted_provider() {
        // Regression test for https://gitcode.com/atomgit_atomcode/atomcode/issues/353
        // User deletes a provider section from config.toml but leaves
        // default_provider pointing at it — startup must still succeed by
        // falling back to a lexicographically-first provider instead of
        // failing with "Provider 'xxx' not found".
        let toml_str = r#"
            default_provider = "AtomGit-Qwen"

            [providers.openai]
            type = "openai"
            api_key = "sk-test"
            model = "gpt-4o"

            [providers.claude]
            type = "claude"
            api_key = "sk-a"
            model = "claude-opus-4-6"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let provider = config.active_provider(None).unwrap();
        // Should fall back to "claude" (lexicographically first)
        assert_eq!(provider.model, "claude-opus-4-6");
    }

    #[test]
    fn active_provider_falls_back_when_override_points_to_deleted_provider() {
        // Same as above but via the --provider CLI override.
        let toml_str = r#"
            default_provider = "openai"

            [providers.openai]
            type = "openai"
            api_key = "sk-test"
            model = "gpt-4o"

            [providers.claude]
            type = "claude"
            api_key = "sk-a"
            model = "claude-opus-4-6"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let provider = config.active_provider(Some("nonexistent")).unwrap();
        // Should fall back to "claude" (lexicographically first)
        assert_eq!(provider.model, "claude-opus-4-6");
    }

    #[test]
    fn active_provider_errors_when_default_deleted_and_no_other_providers() {
        // default_provider points to a deleted section AND there are no
        // other providers — must error (nothing to fall back to).
        let toml_str = r#"
            default_provider = "deleted"
            [providers]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let err = config.active_provider(None).unwrap_err();
        assert!(
            err.to_string().contains("No providers configured"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn vision_preprocessor_provider_defaults_to_none() {
        // Existing config.toml files (pre-feature) must parse cleanly with
        // `vision_preprocessor_provider` defaulting to None — feature is opt-in
        // and absence must not break load.
        let toml_str = r#"
            default_provider = "claude"
            [providers.claude]
            type = "claude"
            model = "claude-sonnet-4-5"
            api_key = "sk-test"
        "#;
        let cfg: Config = toml::from_str(toml_str).expect("parse minimal config");
        assert_eq!(cfg.vision_preprocessor_provider, None);
    }

    #[test]
    fn saved_config_roundtrips_language() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut cfg = Config {
            language: Some(crate::locale::Locale::ZhCn),
            ..Config::with_default_provider("p")
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
                reasoning_effort: None,
                thinking_enabled: None,
                thinking_budget: None,
                skip_tls_verify: false,
                ephemeral: false,
                capable_model: None,
                pricing: None,
            },
        );
        cfg.save(tmp.path()).unwrap();

        let loaded = Config::load(tmp.path()).unwrap();
        assert_eq!(loaded.language, Some(crate::locale::Locale::ZhCn));
    }

    #[test]
    fn config_default_has_no_language() {
        let toml_str = r#"
            default_provider = "test"
            [providers]
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.language, None);
    }

    #[test]
    fn with_default_provider_only_sets_provider_name() {
        let mut cfg = Config::with_default_provider("mock");
        assert_eq!(cfg.default_provider, "mock");

        cfg.default_provider.clear();
        assert_eq!(
            toml::to_string(&cfg).unwrap(),
            toml::to_string(&Config::default()).unwrap()
        );
    }

    #[test]
    fn config_missing_language_field_loads_as_none() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "default_provider = \"foo\"\n[providers]\n").unwrap();
        let loaded = Config::load(tmp.path()).unwrap();
        assert_eq!(loaded.language, None);
    }

    #[test]
    fn vision_preprocessor_provider_round_trips_through_toml() {
        let toml_str = r#"
            default_provider = "claude"
            vision_preprocessor_provider = "AtomGit-Qwen-Qwen3-VL-32B-Instruct"
            [providers.claude]
            type = "claude"
            model = "claude-sonnet-4-5"
            api_key = "sk-test"
        "#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        assert_eq!(
            cfg.vision_preprocessor_provider.as_deref(),
            Some("AtomGit-Qwen-Qwen3-VL-32B-Instruct"),
        );
    }

    /// Helper: minimal Config with one provider, configurable model name +
    /// optional preprocessor key. Used by the can_handle_attached_images tests.
    fn cfg_with(active_model: &str, preprocessor_key: Option<&str>) -> Config {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "active".to_string(),
            crate::config::provider::ProviderConfig {
                provider_type: "openai".into(),
                api_key: Some("sk-test".into()),
                model: active_model.into(),
                base_url: Some("http://127.0.0.1/".into()),
                system_prompt: None,
                user_agent: None,
                context_window: 8000,
                max_tokens: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                thinking_enabled: None,
                thinking_budget: None,
                skip_tls_verify: false,
                ephemeral: false,
                capable_model: None,
                pricing: None,
            },
        );
        Config {
            providers,
            vision_preprocessor_provider: preprocessor_key.map(|s| s.to_string()),
            ..Config::with_default_provider("active")
        }
    }

    #[test]
    fn can_handle_attached_images_true_when_active_provider_accepts_images() {
        // Vision-capable main provider — preprocessor irrelevant.
        let cfg = cfg_with("claude-sonnet-4-5", None);
        assert!(cfg.can_handle_attached_images());
    }

    #[test]
    fn can_handle_attached_images_false_for_text_only_main_and_no_preprocessor() {
        // The original gate's behaviour: refuse paste.
        let cfg = cfg_with("deepseek-v4-flash", None);
        assert!(!cfg.can_handle_attached_images());
    }

    #[test]
    fn can_handle_attached_images_false_when_preprocessor_key_does_not_resolve() {
        // Configured but the key is missing from `providers`. Must NOT
        // accept the paste — the user would just hit `[图片识别失败]` on
        // every send. Better to surface the error at paste time.
        let cfg = cfg_with("deepseek-v4-flash", Some("NoSuchProvider"));
        assert!(!cfg.can_handle_attached_images());
    }

    #[test]
    fn can_handle_attached_images_false_when_preprocessor_key_is_empty_string() {
        let cfg = cfg_with("deepseek-v4-flash", Some(""));
        assert!(!cfg.can_handle_attached_images());
    }

    #[test]
    fn image_attach_support_distinguishes_unconfigured_from_misconfigured() {
        use super::ImageAttachSupport as S;
        // Text-only main, nothing set → Unconfigured.
        assert_eq!(
            cfg_with("deepseek-v4-flash", None).image_attach_support(),
            S::Unconfigured
        );
        // Empty string is treated as unset, not a misconfigured name.
        assert_eq!(
            cfg_with("deepseek-v4-flash", Some("")).image_attach_support(),
            S::Unconfigured
        );
        // Configured but the name doesn't resolve → names the offending value
        // so the gate can say "typo" instead of the misleading "未配置".
        assert_eq!(
            cfg_with("deepseek-v4-flash", Some("NoSuchProvider")).image_attach_support(),
            S::PreprocessorUnresolvable("NoSuchProvider".to_string())
        );
        // Active vision model → Supported regardless of preprocessor.
        assert_eq!(
            cfg_with("claude-sonnet-4-5", None).image_attach_support(),
            S::Supported
        );
    }

    #[test]
    fn can_handle_attached_images_true_when_preprocessor_resolves() {
        // Main is text-only but a preprocessor is configured + present.
        let mut cfg = cfg_with("deepseek-v4-flash", Some("vl-helper"));
        cfg.providers.insert(
            "vl-helper".into(),
            crate::config::provider::ProviderConfig {
                provider_type: "openai".into(),
                api_key: Some("sk-vl".into()),
                model: "Qwen/Qwen3-VL-32B-Instruct".into(),
                base_url: Some("http://127.0.0.1/".into()),
                system_prompt: None,
                user_agent: None,
                context_window: 8000,
                max_tokens: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                thinking_enabled: None,
                thinking_budget: None,
                skip_tls_verify: false,
                ephemeral: false,
                capable_model: None,
                pricing: None,
            },
        );
        assert!(cfg.can_handle_attached_images());
    }
}

#[cfg(test)]
mod reflection_config_tests {
    use super::*;

    #[test]
    fn legacy_reflection_cadence_field_is_silently_ignored() {
        // Older configs in the wild still carry `reflection_cadence = 7`
        // (the field's value at the time the mechanism was removed).
        // toml + serde's default permissiveness means the unknown field
        // is dropped without erroring; this test pins that behaviour so
        // an accidental `#[serde(deny_unknown_fields)]` later doesn't
        // start rejecting users' on-disk configs.
        let toml_text = r#"
default_provider = "claude"
reflection_cadence = 7
[providers]
"#;
        let _cfg: Config = toml::from_str(toml_text).expect("legacy field ignored");
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

    #[test]
    fn saved_config_preserves_explicit_telemetry_section() {
        let tmp = std::env::temp_dir().join(format!(
            "atomcode_cfg_telemetry_rt_{}.toml",
            std::process::id()
        ));
        let cfg = Config {
            default_provider: "p".to_string(),
            telemetry: TelemetryConfig {
                enabled: Some(false),
                endpoint: Some("https://telemetry.example/v1".to_string()),
            },
            ..Config::default()
        };

        cfg.save(&tmp).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(text.contains("[telemetry]"));
        assert!(text.contains("enabled = false"));
        assert!(text.contains("endpoint = \"https://telemetry.example/v1\""));

        let reloaded = Config::load(&tmp).unwrap();
        assert_eq!(reloaded.telemetry.enabled, Some(false));
        assert_eq!(
            reloaded.telemetry.endpoint.as_deref(),
            Some("https://telemetry.example/v1")
        );
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod provider_accounts_model_profiles_tests {
    use super::*;

    const NEW_SCHEMA: &str = r#"
default_provider = ""
default_model = "aliyun-default/qwen3-coder-plus"

[provider_accounts.aliyun-default]
provider = "aliyun"
api_key = "sk-secret"

[models."aliyun-default/qwen3-coder-plus"]
account = "aliyun-default"
model = "qwen3-coder-plus"
context_window = 131072

[models."aliyun-default/qwen3-max"]
account = "aliyun-default"
model = "qwen3-max"
context_window = 131072
capable_model = 5
"#;

    #[test]
    fn new_schema_parses_accounts_models_and_default_model() {
        let cfg: Config = toml::from_str(NEW_SCHEMA).unwrap();
        assert_eq!(cfg.provider_accounts.len(), 1);
        assert_eq!(cfg.provider_accounts["aliyun-default"].provider, "aliyun");
        assert_eq!(cfg.models.len(), 2);
        assert_eq!(
            cfg.models["aliyun-default/qwen3-coder-plus"].model,
            "qwen3-coder-plus"
        );
        assert_eq!(
            cfg.models["aliyun-default/qwen3-max"].capable_model,
            Some(5)
        );
        assert_eq!(
            cfg.default_model.as_deref(),
            Some("aliyun-default/qwen3-coder-plus")
        );
    }

    #[test]
    fn new_schema_round_trips_through_serialize() {
        let cfg: Config = toml::from_str(NEW_SCHEMA).unwrap();
        let rendered = cfg.serialize_for_disk(None).unwrap();
        let reparsed: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(reparsed.default_model, cfg.default_model);
        assert_eq!(reparsed.provider_accounts.len(), 1);
        assert_eq!(reparsed.models.len(), 2);
        assert_eq!(
            reparsed.models["aliyun-default/qwen3-max"].context_window,
            131072
        );
    }

    #[test]
    fn ephemeral_account_and_its_models_are_not_serialized() {
        let mut cfg: Config = toml::from_str(NEW_SCHEMA).unwrap();
        cfg.provider_accounts.insert(
            "oauth-live".into(),
            provider::ProviderAccountConfig {
                provider: "openai".into(),
                display_name: None,
                api_key: Some("sk-runtime-only".into()),
                base_url: None,
                user_agent: None,
                skip_tls_verify: false,
                enterprise_url: None,
                ephemeral: true,
            },
        );
        cfg.models.insert(
            "oauth-live/gpt".into(),
            provider::ModelProfileConfig {
                account: "oauth-live".into(),
                model: "gpt-x".into(),
                display_name: None,
                system_prompt: None,
                context_window: 128_000,
                max_tokens: None,
                capable_model: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                thinking_enabled: None,
                thinking_budget: None,
                pricing: None,
            },
        );
        let rendered = cfg.serialize_for_disk(None).unwrap();
        assert!(
            !rendered.contains("sk-runtime-only"),
            "ephemeral account credential leaked into saved config"
        );
        assert!(
            !rendered.contains("oauth-live"),
            "ephemeral account persisted"
        );
        // The persistent account + models survive.
        assert!(rendered.contains("aliyun-default"));
    }

    #[test]
    fn validation_catches_bad_references_and_limits() {
        let mut cfg = Config::default();
        cfg.provider_accounts.insert(
            "corp".into(),
            provider::ProviderAccountConfig {
                provider: "openai-compatible".into(), // no default endpoint…
                display_name: None,
                api_key: None,
                base_url: None, // …and none supplied → error
                user_agent: None,
                skip_tls_verify: false,
                enterprise_url: None,
                ephemeral: false,
            },
        );
        cfg.models.insert(
            "corp/bad".into(),
            provider::ModelProfileConfig {
                account: "does-not-exist".into(), // dangling reference → error
                model: "".into(),                 // empty model → error
                display_name: None,
                system_prompt: None,
                context_window: 0, // zero window → error
                max_tokens: None,
                capable_model: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                thinking_enabled: None,
                thinking_budget: None,
                pricing: None,
            },
        );
        cfg.default_model = Some("nope".into()); // unresolvable default → error

        let diags = cfg.validate_provider_accounts_and_models();
        assert!(
            diags.iter().any(|d| d.contains("no default endpoint")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.contains("unknown account")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.contains("missing `model`")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.contains("context_window = 0")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.contains("default_model")),
            "{diags:?}"
        );
    }

    #[test]
    fn valid_new_schema_passes_validation() {
        let cfg: Config = toml::from_str(NEW_SCHEMA).unwrap();
        assert!(
            cfg.validate_provider_accounts_and_models().is_empty(),
            "{:?}",
            cfg.validate_provider_accounts_and_models()
        );
    }
}

#[cfg(test)]
mod legacy_projection_tests {
    use super::*;

    const LEGACY: &str = r#"
default_provider = "MyDeepSeek"

[providers.MyDeepSeek]
type = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "sk-legacy"
model = "deepseek-chat"
context_window = 128000
capable_model = 3
"#;

    #[test]
    fn legacy_provider_projects_to_synthetic_account_and_model() {
        let cfg: Config = toml::from_str(LEGACY).unwrap();
        let accounts = cfg.logical_accounts();
        let a = accounts.get("MyDeepSeek").expect("synthetic account");
        assert_eq!(a.provider, "openai");
        assert_eq!(a.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
        assert_eq!(a.api_key.as_deref(), Some("sk-legacy"));

        let models = cfg.logical_models();
        let m = models.get("MyDeepSeek").expect("synthetic model");
        assert_eq!(m.account, "MyDeepSeek");
        assert_eq!(m.model, "deepseek-chat");
        assert_eq!(m.context_window, 128000);
        assert_eq!(m.capable_model, Some(3));

        assert_eq!(
            cfg.effective_model_selection().as_deref(),
            Some("MyDeepSeek")
        );
    }

    #[test]
    fn mixed_schema_catalog_includes_legacy_and_new() {
        let toml = format!(
            "{LEGACY}\n[provider_accounts.corp]\nprovider = \"openai-compatible\"\nbase_url = \"https://llm.corp/v1\"\n\n[models.\"corp/code\"]\naccount = \"corp\"\nmodel = \"corp-code\"\ncontext_window = 200000\n"
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        let accounts = cfg.logical_accounts();
        assert!(accounts.contains_key("MyDeepSeek"), "legacy projected");
        assert!(accounts.contains_key("corp"), "new-schema account");
        let models = cfg.logical_models();
        assert!(models.contains_key("MyDeepSeek"));
        assert!(models.contains_key("corp/code"));
        assert!(cfg.validate_provider_accounts_and_models().is_empty());
    }

    #[test]
    fn new_schema_wins_on_id_collision_with_diagnostic() {
        let toml = r#"
default_provider = "dup"

[providers.dup]
type = "openai"
base_url = "https://legacy/v1"
model = "legacy-model"
context_window = 64000

[provider_accounts.dup]
provider = "deepseek"

[models.dup]
account = "dup"
model = "new-model"
context_window = 131072
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        // New-schema entries win.
        assert_eq!(cfg.logical_accounts()["dup"].provider, "deepseek");
        assert_eq!(cfg.logical_models()["dup"].model, "new-model");
        // …and the collision is reported, not silent.
        let diags = cfg.model_catalog_collisions();
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert!(diags.iter().all(|d| d.contains("dup")));
    }

    #[test]
    fn effective_selection_prefers_default_model_over_default_provider() {
        let toml = "default_provider = \"X\"\ndefault_model = \"acc/y\"\n\n[provider_accounts.acc]\nprovider = \"openai\"\n\n[models.\"acc/y\"]\naccount = \"acc\"\nmodel = \"y\"\ncontext_window = 8000\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.effective_model_selection().as_deref(), Some("acc/y"));
    }

    #[test]
    fn upgrade_legacy_provider_moves_entry_and_repoints_default() {
        let mut cfg: Config = toml::from_str(LEGACY).unwrap();
        cfg.upgrade_legacy_provider("MyDeepSeek").unwrap();
        assert!(!cfg.providers.contains_key("MyDeepSeek"), "legacy removed");
        assert!(cfg.provider_accounts.contains_key("MyDeepSeek"));
        assert!(cfg.models.contains_key("MyDeepSeek"));
        assert_eq!(cfg.default_model.as_deref(), Some("MyDeepSeek"));
        // Unknown / colliding upgrades error rather than corrupt.
        assert!(cfg.upgrade_legacy_provider("nope").is_err());
        let mut legacy2: Config = toml::from_str(LEGACY).unwrap();
        legacy2.provider_accounts.insert(
            "MyDeepSeek".into(),
            provider::ProviderAccountConfig {
                provider: "deepseek".into(),
                display_name: None,
                api_key: None,
                base_url: None,
                user_agent: None,
                skip_tls_verify: false,
                enterprise_url: None,
                ephemeral: false,
            },
        );
        assert!(legacy2.upgrade_legacy_provider("MyDeepSeek").is_err());
    }

    #[test]
    fn model_referencing_a_legacy_provider_account_validates_and_resolves() {
        // A new-schema model may point its `account` at a legacy provider name
        // (which projects to a synthetic account). Validation must accept it and
        // resolution must succeed.
        let toml = "default_model = \"leg/extra\"\n\n[providers.leg]\ntype = \"openai\"\nbase_url = \"https://api.deepseek.com/v1\"\napi_key = \"sk-leg\"\nmodel = \"deepseek-chat\"\ncontext_window = 128000\n\n[models.\"leg/extra\"]\naccount = \"leg\"\nmodel = \"deepseek-coder\"\ncontext_window = 131072\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(
            cfg.validate_provider_accounts_and_models().is_empty(),
            "{:?}",
            cfg.validate_provider_accounts_and_models()
        );
        let r = cfg.resolve_model(None).unwrap();
        assert_eq!(r.model, "deepseek-coder");
        assert_eq!(r.api_key.as_deref(), Some("sk-leg"));
        assert_eq!(r.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
    }

    #[test]
    fn legacy_only_config_is_not_rewritten_on_serialize() {
        let cfg: Config = toml::from_str(LEGACY).unwrap();
        let rendered = cfg.serialize_for_disk(None).unwrap();
        assert!(
            rendered.contains("[providers.MyDeepSeek]"),
            "legacy section kept verbatim"
        );
        assert!(
            !rendered.contains("[provider_accounts"),
            "load/serialize must not auto-upgrade legacy into the new schema"
        );
    }

    #[test]
    fn provider_config_for_selection_covers_legacy_and_new_schema() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "providers": { "leg": { "type": "openai", "base_url": "https://legacy/v1", "model": "m", "context_window": 8000 } },
            "provider_accounts": { "acc": { "provider": "deepseek", "base_url": "https://mirror/v1" } },
            "models": { "acc/ds": { "account": "acc", "model": "deepseek-chat", "context_window": 131072 } }
        }))
        .unwrap();
        // Legacy provider returned verbatim.
        let leg = cfg.provider_config_for_selection("leg").unwrap();
        assert_eq!(leg.model, "m");
        assert_eq!(leg.base_url.as_deref(), Some("https://legacy/v1"));
        // New-schema model id reconstructs a ProviderConfig via resolve_model.
        let new = cfg.provider_config_for_selection("acc/ds").unwrap();
        assert_eq!(new.model, "deepseek-chat");
        assert_eq!(new.base_url.as_deref(), Some("https://mirror/v1"));
        assert_eq!(new.context_window, 131072);
        assert!(!new.ephemeral);
        // Unknown id → None.
        assert!(cfg.provider_config_for_selection("nope").is_none());
        assert!(cfg.selection_exists("leg") && cfg.selection_exists("acc/ds"));
        assert!(!cfg.selection_exists("nope"));
    }

    #[test]
    fn active_provider_resolves_new_schema_when_providers_empty() {
        // A CodingPlan-style config: everything in the new schema, no legacy
        // `[providers.*]`. active_provider must still resolve (regression: it
        // used to read only config.providers → Err → footer "未配置").
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "default_model": "AtomGit-deepseek-v4-flash",
            "provider_accounts": { "AtomGit": { "provider": "openai", "base_url": "https://llm-api.atomgit.com/v1" } },
            "models": { "AtomGit-deepseek-v4-flash": { "account": "AtomGit", "model": "deepseek-v4-flash", "context_window": 128000 } }
        }))
        .unwrap();
        assert!(cfg.providers.is_empty());
        let p = cfg.active_provider(None).unwrap();
        assert_eq!(p.model, "deepseek-v4-flash");
        assert_eq!(
            p.base_url.as_deref(),
            Some("https://llm-api.atomgit.com/v1")
        );
        // Falls back to a catalog model when the selection is dangling.
        let p2 = cfg.active_provider(Some("nope")).unwrap();
        assert_eq!(p2.model, "deepseek-v4-flash");
    }

    #[test]
    fn update_selection_reasoning_writes_to_correct_schema() {
        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "providers": { "leg": { "type": "openai", "base_url": "https://legacy/v1", "model": "m", "context_window": 8000 } },
            "provider_accounts": { "acc": { "provider": "deepseek" } },
            "models": { "acc/ds": { "account": "acc", "model": "deepseek-chat", "context_window": 131072 } }
        }))
        .unwrap();
        // New-schema model — covers the full reasoning/thinking field set.
        assert!(cfg.update_selection_reasoning("acc/ds", |r| {
            *r.thinking_enabled = Some(true);
            *r.reasoning_effort = Some("high".into());
            *r.thinking_type = Some("enabled".into());
            *r.thinking_keep = Some("all".into());
        }));
        assert_eq!(cfg.models["acc/ds"].thinking_enabled, Some(true));
        assert_eq!(
            cfg.models["acc/ds"].reasoning_effort.as_deref(),
            Some("high")
        );
        assert_eq!(
            cfg.models["acc/ds"].thinking_type.as_deref(),
            Some("enabled")
        );
        assert_eq!(cfg.models["acc/ds"].thinking_keep.as_deref(), Some("all"));
        // Legacy provider.
        assert!(cfg.update_selection_reasoning("leg", |r| *r.thinking_budget = Some(2048)));
        assert_eq!(cfg.providers["leg"].thinking_budget, Some(2048));
        // Unknown id → false, no write.
        assert!(!cfg.update_selection_reasoning("nope", |r| *r.thinking_enabled = Some(false)));
    }

    #[test]
    fn codingplan_flat_providers_fold_into_grouped_accounts() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "providers": {
                "AtomGit-GLM-5.2": { "type": "openai", "base_url": "https://llm-api.atomgit.com/v1", "model": "GLM-5.2", "context_window": 64000 },
                "AtomGit-Qwen": { "type": "openai", "base_url": "https://llm-api.atomgit.com/v1", "model": "Qwen", "context_window": 64000 },
                "AtomGit-anthropic-claude": { "type": "claude", "base_url": "https://llm-api.atomgit.com/v1", "model": "claude-3.5", "context_window": 200000 },
                "my-openai": { "type": "openai", "base_url": "https://api.openai.com/v1", "model": "gpt-4", "context_window": 128000 }
            }
        }))
        .unwrap();
        let accounts = cfg.logical_accounts();
        // Two openai CodingPlan models collapse into ONE account; claude gets its
        // own; the user's manual provider is untouched.
        assert!(accounts.contains_key("AtomGit"));
        assert!(accounts.contains_key("AtomGit-anthropic"));
        assert!(accounts.contains_key("my-openai"));
        assert!(!accounts.contains_key("AtomGit-GLM-5.2"), "folded away");
        assert!(!accounts.contains_key("AtomGit-Qwen"), "folded away");

        let models = cfg.logical_models();
        // Model ids stay = legacy provider keys (default_provider stays resolvable),
        // only the parent account folds.
        assert_eq!(models["AtomGit-GLM-5.2"].account, "AtomGit");
        assert_eq!(models["AtomGit-Qwen"].account, "AtomGit");
        assert_eq!(
            models["AtomGit-anthropic-claude"].account,
            "AtomGit-anthropic"
        );
        assert_eq!(models["my-openai"].account, "my-openai");

        // Resolving by the stable legacy id still works and keeps the gateway
        // base_url (so the OAuth request signer still fires).
        let r = cfg.resolve_model(Some("AtomGit-GLM-5.2")).unwrap();
        assert_eq!(r.account_id, "AtomGit");
        assert_eq!(r.model, "GLM-5.2");
        assert_eq!(r.provider_type, "openai");
        assert!(r.base_url.as_deref().unwrap().contains("atomgit"));
        let c = cfg.resolve_model(Some("AtomGit-anthropic-claude")).unwrap();
        assert_eq!(c.account_id, "AtomGit-anthropic");
        assert_eq!(c.provider_type, "anthropic");
    }

    #[test]
    fn resolve_model_resolves_a_legacy_selection() {
        let cfg: Config = toml::from_str(LEGACY).unwrap();
        let r = cfg.resolve_model(None).unwrap();
        assert_eq!(r.selection_id, "MyDeepSeek");
        assert_eq!(r.account_id, "MyDeepSeek");
        assert_eq!(r.provider_type, "openai");
        assert_eq!(r.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
        assert_eq!(r.api_key.as_deref(), Some("sk-legacy"));
        assert_eq!(r.model, "deepseek-chat");
        assert_eq!(r.context_window, 128000);
        assert_eq!(r.capable_model, Some(3));
    }

    #[test]
    fn resolve_model_uses_preset_default_base_url_when_account_omits_it() {
        let toml = "default_model = \"acc/ds\"\n\n[provider_accounts.acc]\nprovider = \"deepseek\"\napi_key = \"sk-x\"\n\n[models.\"acc/ds\"]\naccount = \"acc\"\nmodel = \"deepseek-chat\"\ncontext_window = 131072\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        let r = cfg.resolve_model(None).unwrap();
        assert_eq!(r.provider_id, "deepseek");
        assert_eq!(r.provider_type, "openai");
        // Falls back to the deepseek preset's default endpoint.
        assert_eq!(r.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
    }

    #[test]
    fn account_base_url_overrides_preset_default() {
        let toml = "default_model = \"acc/ds\"\n\n[provider_accounts.acc]\nprovider = \"deepseek\"\nbase_url = \"https://mirror.internal/v1\"\n\n[models.\"acc/ds\"]\naccount = \"acc\"\nmodel = \"deepseek-chat\"\ncontext_window = 131072\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.resolve_model(None).unwrap().base_url.as_deref(),
            Some("https://mirror.internal/v1")
        );
    }

    #[test]
    fn resolve_model_errors_are_secret_safe() {
        // A resolution failure must never echo an account credential.
        let toml = "default_model = \"missing\"\n\n[provider_accounts.acc]\nprovider = \"openai\"\napi_key = \"sk-SUPER-SECRET-XYZ\"\n\n[models.\"acc/m\"]\naccount = \"acc\"\nmodel = \"gpt\"\ncontext_window = 8000\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        let err = cfg.resolve_model(None).unwrap_err().to_string();
        assert!(err.contains("missing"), "{err}");
        assert!(
            !err.contains("SUPER-SECRET"),
            "credential leaked in error: {err}"
        );
    }

    #[test]
    fn resolve_model_errors_without_a_selection() {
        let cfg = Config::default(); // no default_model, empty default_provider
        assert!(cfg.resolve_model(None).is_err());
    }

    #[test]
    fn default_context_window_routes_through_resolution() {
        // Legacy: equals providers[default_provider].context_window (128000).
        let legacy: Config = toml::from_str(LEGACY).unwrap();
        assert_eq!(legacy.default_context_window(), 128000);
        // New schema: equals the selected model profile's window.
        let toml = "default_model = \"acc/ds\"\n\n[provider_accounts.acc]\nprovider = \"deepseek\"\napi_key = \"sk-x\"\n\n[models.\"acc/ds\"]\naccount = \"acc\"\nmodel = \"deepseek-chat\"\ncontext_window = 200000\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.default_context_window(), 200000);
    }
}
