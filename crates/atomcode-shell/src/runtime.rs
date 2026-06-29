//! Neutral driver helpers for building a kernel coding runtime's provider.
//!
//! The bridge's TRANSLATION runtime (the `Bridge` state machine that presented the new
//! stack behind `core`'s legacy `AgentClient`/`AgentEvent` protocol) is GONE — tuix now
//! consumes the kernel natively (its own `native` adapter) and the daemon was always
//! native (`CodingRuntime::spawn`). What remains here is the small, neutral glue both
//! drivers (cli + daemon) still share: a driver-supplied [`ShellConfig`], its mapping to
//! a [`CodingAgentConfig`] ([`coding_config`]), and provider construction with the
//! AtomGit signing gateway ([`build_provider`], via [`crate::sign`]). `build_provider`
//! can't live in `atomcode-coding` (it uses `atomcode_core::coding_plan::crypto` signing,
//! and coding is core-neutral by design), so it stays on the driver side here.

use std::path::PathBuf;
use std::sync::Arc;

use atomcode_capabilities::codeintel::{LspServerSetting, LspSettings};
use atomcode_coding::CodingAgentConfig;

/// What a driver supplies to build the new-stack coding runtime. Resolved by the CALLER
/// (the cli / daemon already have a loaded `Config`) so this crate stays
/// config-format-agnostic.
#[derive(Clone)]
pub struct ShellConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub working_dir: PathBuf,
    pub context_window: u32,
    /// Disable MCP connection (mirrors the legacy `--no-mcp` style switches).
    pub mcp: bool,
    /// Telemetry sink forwarded to the coding assembly (→ a `LlmChat`-emitting
    /// hook). `None` ⇒ no telemetry. The driver supplies its own `Telemetry`.
    pub telemetry: Option<std::sync::Arc<atomcode_telemetry::Telemetry>>,
    /// Provider `reasoning_history` override (`"include"` | `"exclude"`) from the
    /// driver's provider config. `None` ⇒ the adapter auto-detects by model.
    pub reasoning_history: Option<String>,
    /// Provider `reasoning_effort` override (`"low"|"medium"|"high"|"max"`) from the
    /// driver's provider config (the `/effort` control writes it). `None`/`"off"` ⇒ no
    /// opinion. Threaded into `ChatOptions.reasoning_effort` so v2 honors `/effort`.
    pub reasoning_effort: Option<String>,
    /// Provider adapter kind (`"openai"` | `"claude"` | `"ollama"`). Selects the v2 adapter
    /// the engine builds.
    pub provider_type: String,
    /// `/think on|off` → Anthropic (adaptive) / Ollama thinking toggle.
    pub thinking_enabled: Option<bool>,
    /// Kimi-family `thinking.type` for OpenAI-compatible models.
    pub thinking_type: Option<String>,
    /// Kimi K2.6 `thinking.keep`.
    pub thinking_keep: Option<String>,
    /// `--dangerously-skip-permissions` / `-y`: auto-approve every tool without a
    /// prompt. The driver honors it (tuix's native adapter / the daemon); threaded here
    /// so the knob reaches the coding config.
    pub dangerously_skip_permissions: bool,
    /// Is a human present to answer approval prompts? `true` (interactive TUI / live web)
    /// ⇒ approvals PARK until answered. `false` (headless `-p`, automated) ⇒ keep the
    /// fail-closed approval timeout. Maps to the kernel agent's `request_timeout`.
    pub interactive: bool,
    /// LSP diagnostics policy mapped from the driver's `[lsp]` config (off by default).
    pub lsp: LspSettings,
    /// User-configured per-response output cap (`[providers.*] max_tokens`). `None` ⇒ the
    /// adapter falls back to [`default_max_tokens`] of the context window. Threaded into the
    /// per-call `ChatOptions` (which wins over the fallback).
    pub max_tokens: Option<u32>,
    /// `web_search` backend from `[web_search] provider` — `Some` only when the user picked a
    /// NON-default backend (so the `ATOMCODE_WEB_SEARCH_PROVIDER` env fallback still applies
    /// for the default). `[web_search] api_key` for the Exa backend (env `EXA_API_KEY` wins).
    pub web_search_provider: Option<String>,
    pub web_search_api_key: Option<String>,
}

/// Map core's `[lsp]` config DTO to the neutral [`LspSettings`] the coding/capabilities
/// layers consume — the one place that bridges `atomcode_core::config` to L1 (which may not
/// depend on core). Field-for-field; the per-extension server map is copied verbatim.
fn lsp_settings_from(lsp: &atomcode_core::config::LspConfig) -> LspSettings {
    LspSettings {
        enabled: lsp.enabled,
        auto_detect: lsp.auto_detect,
        settle_delay_ms: lsp.diagnostics_settle_delay_ms,
        servers: lsp
            .servers
            .iter()
            .map(|(ext, s)| {
                (
                    ext.clone(),
                    LspServerSetting {
                        command: s.command.clone(),
                        args: s.args.clone(),
                        root_markers: s.root_markers.clone(),
                    },
                )
            })
            .collect(),
    }
}

impl ShellConfig {
    /// Build a [`ShellConfig`] from a resolved provider config — the SINGLE place the
    /// driver→config field mapping lives, so the cli + daemon construction sites can't
    /// drift (the ShellConfig-drops-per-provider-config footgun). `p == None` ⇒ neutral
    /// defaults (empty creds, 128K window, `"openai"` adapter), mirroring the drivers'
    /// prior `unwrap_or_default` behaviour for an unknown provider. Provider RESOLUTION
    /// stays with the caller (cli's `active_provider` vs daemon's `providers.get` differ).
    pub fn from_provider(
        p: Option<&atomcode_core::config::provider::ProviderConfig>,
        lsp: &atomcode_core::config::LspConfig,
        web_search: &atomcode_core::config::WebSearchConfig,
        working_dir: &std::path::Path,
        telemetry: Option<Arc<atomcode_telemetry::Telemetry>>,
        dangerously_skip_permissions: bool,
        interactive: bool,
    ) -> Self {
        ShellConfig {
            api_key: p.and_then(|p| p.api_key.clone()).unwrap_or_default(),
            base_url: p.and_then(|p| p.base_url.clone()).unwrap_or_default(),
            model: p.map(|p| p.model.clone()).unwrap_or_default(),
            working_dir: working_dir.to_path_buf(),
            context_window: p.map(|p| p.context_window as u32).unwrap_or(128_000),
            mcp: true,
            telemetry,
            reasoning_history: p.and_then(|p| p.reasoning_history.clone()),
            reasoning_effort: p.and_then(|p| p.reasoning_effort.clone()),
            provider_type: p
                .map(|p| p.provider_type.clone())
                .unwrap_or_else(|| "openai".into()),
            thinking_enabled: p.and_then(|p| p.thinking_enabled),
            thinking_type: p.and_then(|p| p.thinking_type.clone()),
            thinking_keep: p.and_then(|p| p.thinking_keep.clone()),
            dangerously_skip_permissions,
            interactive,
            lsp: lsp_settings_from(lsp),
            max_tokens: p.and_then(|p| p.max_tokens).map(|m| m as u32),
            // Only a NON-default provider is authoritative; the default ("exa") is left None
            // so the ATOMCODE_WEB_SEARCH_PROVIDER env fallback is not shadowed.
            web_search_provider: (web_search.provider != "exa").then(|| web_search.provider.clone()),
            web_search_api_key: web_search.api_key.clone(),
        }
    }
}

/// Map a driver-supplied [`ShellConfig`] to the [`CodingAgentConfig`] the new stack
/// assembles from. PUBLIC so every native driver (cli, daemon, the headless CLI) reuses
/// the EXACT same knob mapping — no divergence (the ShellConfig-drops-per-provider-config
/// footgun).
pub fn coding_config(cfg: &ShellConfig) -> CodingAgentConfig {
    let mut coding_cfg =
        CodingAgentConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model, &cfg.working_dir);
    coding_cfg.context_window = cfg.context_window;
    coding_cfg.telemetry = cfg.telemetry.clone();
    coding_cfg.reasoning_history = cfg.reasoning_history.clone();
    // `/effort`: thread the per-provider reasoning_effort into the per-call ChatOptions
    // so v2 actually emits it (openai_compat → `reasoning_effort` body field).
    coding_cfg.chat_options.reasoning_effort =
        atomcode_kernel::provider::ReasoningEffort::from_config(cfg.reasoning_effort.as_deref());
    // Adapter selection + thinking controls (so Claude-/Ollama-native + /think work in v2).
    coding_cfg.provider_type = cfg.provider_type.clone();
    coding_cfg.thinking_enabled = cfg.thinking_enabled;
    coding_cfg.thinking_type = cfg.thinking_type.clone();
    coding_cfg.thinking_keep = cfg.thinking_keep.clone();
    // Per-call output cap from the user's `[providers.*] max_tokens`. The provider adapter
    // does `options.max_tokens.or(cfg.max_tokens)`, so this wins over the build_provider
    // fallback; `None` here ⇒ the fallback applies.
    coding_cfg.chat_options.max_tokens = cfg.max_tokens;
    // `[web_search]` provider/key (the tool reads these; env still fills the gaps).
    coding_cfg.web_search_provider = cfg.web_search_provider.clone();
    coding_cfg.web_search_api_key = cfg.web_search_api_key.clone();
    // Interactive drivers PARK approvals (a present human must not be auto-denied for
    // thinking too long); headless keeps the configured fail-closed timeout.
    if cfg.interactive {
        coding_cfg.request_timeout = None;
    }
    coding_cfg.lsp = cfg.lsp.clone();
    coding_cfg
}

/// Fallback per-response output cap when the user sets no `[providers.*] max_tokens`.
/// Mirrors the legacy v1 engine + the deleted bridge: a quarter of the context window,
/// clamped to `[8_000, 16_384]`. Without it, OpenAI-compat/gateway requests send NO
/// `max_tokens` (the gateway then imposes its own small hidden cap → frequent
/// `finish_reason=length` truncation) and Anthropic falls to its flat 4096 default.
fn default_max_tokens(context_window: u32) -> u32 {
    (context_window / 4).clamp(8_000, 16_384)
}

pub fn build_provider(
    cfg: &CodingAgentConfig,
) -> anyhow::Result<Arc<dyn atomcode_kernel::provider::LlmProvider>> {
    use atomcode_capabilities::provider::{
        AnthropicConfig, AnthropicProvider, OllamaConfig, OllamaProvider, OpenAiCompatConfig,
        OpenAiCompatProvider, ReasoningPolicy,
    };
    use atomcode_core::coding_plan::crypto;

    // Dispatch by provider_type — the v2 engine has native adapters for each, and using the
    // wrong one (e.g. OpenAI-format to the Anthropic API) fails. Mirrors v1 `create_provider`.
    match cfg.provider_type.as_str() {
        "claude" | "anthropic" => {
            let mut ac = AnthropicConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
            ac.context_window = cfg.context_window;
            // Fallback output cap (the per-call `chat_options.max_tokens` still wins). Replaces
            // the flat 4096 default so a large context window gets a proportionate cap.
            ac.max_tokens = default_max_tokens(cfg.context_window);
            // `/think on` → adaptive extended thinking. (v2 uses adaptive, so v1's
            // thinking_budget has no direct mapping — intentionally dropped.)
            ac.thinking = cfg.thinking_enabled.unwrap_or(false);
            Ok(Arc::new(
                AnthropicProvider::new(ac).map_err(|e| anyhow::anyhow!(e.message))?,
            ))
        }
        "ollama" => {
            let mut oc = OllamaConfig::new(&cfg.base_url, &cfg.model);
            oc.api_key = cfg.api_key.clone();
            oc.context_window = cfg.context_window;
            // Fallback `num_predict` cap (the per-call `chat_options.max_tokens` still wins).
            oc.max_tokens = Some(default_max_tokens(cfg.context_window));
            oc.think = cfg.thinking_enabled.unwrap_or(false);
            Ok(Arc::new(
                OllamaProvider::new(oc).map_err(|e| anyhow::anyhow!(e.message))?,
            ))
        }
        // "openai" (default) + any unknown → OpenAI-compatible.
        _ => {
            let mut pc = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
            pc.context_window = cfg.context_window;
            // Fallback output cap so the gateway can't impose its own small hidden cap (the
            // per-call `chat_options.max_tokens` still wins via `options.or(cfg)`).
            pc.max_tokens = Some(default_max_tokens(cfg.context_window));
            // Honor the provider's `reasoning_history` override; unset ⇒ leave `None` so the
            // adapter auto-detects by model. A typo fails fast (parity with the legacy engine).
            pc.reasoning_policy = ReasoningPolicy::from_config(cfg.reasoning_history.as_deref())
                .map_err(|e| anyhow::anyhow!(e))?;
            // Kimi-family thinking (`thinking.{type,keep}`); omitted unless configured.
            pc.thinking_type = cfg.thinking_type.clone();
            pc.thinking_keep = cfg.thinking_keep.clone();

            // AtomGit gateways need per-request auth instead of a static api_key, handled by
            // the closed `atomcode-codingplan-crypto` (gated by core's `codingplan-crypto`
            // feature). Open-source builds have none → fail fast with an actionable message.
            if crypto::is_atomgit_gateway(&cfg.base_url) {
                if !crypto::signer_available() {
                    anyhow::bail!(
                        "{}",
                        atomcode_core::i18n::t(atomcode_core::i18n::Msg::GatewayAuthUnavailable {
                            base_url: &cfg.base_url,
                        })
                    );
                }
                pc.request_signer = Some(crate::sign::atomgit_signer(&cfg.base_url)?);
            }

            Ok(Arc::new(
                OpenAiCompatProvider::new(pc).map_err(|e| anyhow::anyhow!(e.message))?,
            ))
        }
    }
}

/// The standard [`ProviderFactory`] every native driver injects into
/// `CodingRuntime::spawn`: construct the provider via [`build_provider`] (incl. the
/// AtomGit signing gateway), surfacing errors as `String` for the kernel's factory slot.
pub fn provider_factory() -> atomcode_coding::ProviderFactory {
    Box::new(|c: &CodingAgentConfig| build_provider(c).map_err(|e| e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{build_provider, coding_config, default_max_tokens, ShellConfig};
    use atomcode_coding::CodingAgentConfig;

    #[test]
    fn coding_config_maps_bridge_knobs_without_divergence() {
        let shell_cfg = ShellConfig {
            api_key: "k".into(),
            base_url: "https://example.test/v1".into(),
            model: "m".into(),
            working_dir: "/tmp".into(),
            context_window: 64_000,
            mcp: true,
            telemetry: None,
            reasoning_history: Some("include".into()),
            reasoning_effort: Some("high".into()),
            provider_type: "claude".into(),
            thinking_enabled: Some(true),
            thinking_type: None,
            thinking_keep: None,
            dangerously_skip_permissions: false,
            interactive: true,
            lsp: Default::default(),
            max_tokens: None,
            web_search_provider: None,
            web_search_api_key: None,
        };
        let cc = coding_config(&shell_cfg);
        assert_eq!(cc.provider_type, "claude");
        assert_eq!(cc.context_window, 64_000);
        assert_eq!(cc.model, "m");
        assert_eq!(cc.reasoning_history.as_deref(), Some("include"));
        assert_eq!(cc.thinking_enabled, Some(true));
        // interactive ⇒ no fail-closed approval timeout (a present human isn't auto-denied).
        assert_eq!(cc.request_timeout, None);
    }

    #[test]
    fn lsp_config_flows_from_core_through_to_coding() {
        // The user's `[lsp]` must reach the coding config (it was previously inert).
        let mut lsp = atomcode_core::config::LspConfig::default();
        lsp.enabled = true;
        lsp.auto_detect = true;
        lsp.diagnostics_settle_delay_ms = 222;
        lsp.servers.insert(
            "rb".into(),
            atomcode_core::config::LspServerConfig {
                command: "solargraph".into(),
                args: vec!["stdio".into()],
                root_markers: vec![],
            },
        );
        let shell = ShellConfig::from_provider(None, &lsp, &ws_default(), std::path::Path::new("/tmp"), None, false, true);
        let cc = coding_config(&shell);
        assert!(cc.lsp.enabled);
        assert!(cc.lsp.auto_detect);
        assert_eq!(cc.lsp.settle_delay_ms, 222);
        assert_eq!(cc.lsp.servers.get("rb").unwrap().command, "solargraph");
        assert_eq!(cc.lsp.servers.get("rb").unwrap().args, vec!["stdio".to_string()]);
    }

    #[test]
    fn lsp_defaults_to_disabled() {
        let lsp = atomcode_core::config::LspConfig::default();
        let shell =
            ShellConfig::from_provider(None, &lsp, &ws_default(), std::path::Path::new("/tmp"), None, false, false);
        assert!(!coding_config(&shell).lsp.enabled, "default config ⇒ LSP off");
    }

    fn ws_default() -> atomcode_core::config::WebSearchConfig {
        atomcode_core::config::WebSearchConfig { provider: "exa".into(), api_key: None }
    }

    #[test]
    fn web_search_non_default_provider_and_key_flow_to_coding() {
        // A non-default `[web_search] provider` (+ api_key) must reach the coding config so
        // the tool honors it (it was previously inert — only env vars worked).
        let lsp = atomcode_core::config::LspConfig::default();
        let ws = atomcode_core::config::WebSearchConfig {
            provider: "duckduckgo".into(),
            api_key: Some("exa-key".into()),
        };
        let shell =
            ShellConfig::from_provider(None, &lsp, &ws, std::path::Path::new("/w"), None, false, false);
        let cc = coding_config(&shell);
        assert_eq!(cc.web_search_provider.as_deref(), Some("duckduckgo"));
        assert_eq!(cc.web_search_api_key.as_deref(), Some("exa-key"));
    }

    #[test]
    fn web_search_default_provider_left_none_to_preserve_env() {
        // Default provider ("exa") ⇒ leave `web_search_provider` None so the
        // ATOMCODE_WEB_SEARCH_PROVIDER env fallback still applies (no shadowing).
        let lsp = atomcode_core::config::LspConfig::default();
        let shell =
            ShellConfig::from_provider(None, &lsp, &ws_default(), std::path::Path::new("/w"), None, false, false);
        assert_eq!(coding_config(&shell).web_search_provider, None);
    }

    fn coding_cfg(reasoning_history: Option<&str>) -> CodingAgentConfig {
        // A plain (non-AtomGit) OpenAI-compatible endpoint so build_provider takes the
        // no-signer path and constructs offline (no network).
        let mut c = CodingAgentConfig::new("sk-x", "https://api.example.com/v1", "some-model", "/tmp");
        c.reasoning_history = reasoning_history.map(str::to_string);
        c
    }

    #[test]
    fn build_provider_honors_reasoning_history_and_rejects_typos() {
        // Valid override → provider builds.
        assert!(build_provider(&coding_cfg(Some("exclude"))).is_ok());
        assert!(build_provider(&coding_cfg(Some("include"))).is_ok());
        // Unset → adapter auto-detects; still builds.
        assert!(build_provider(&coding_cfg(None)).is_ok());
        // Typo → fail fast (parity with the legacy engine's load-time validation).
        let res = build_provider(&coding_cfg(Some("sometimes")));
        assert!(res.is_err(), "a reasoning_history typo must fail provider construction");
        let err = res.err().unwrap().to_string();
        assert!(err.contains("reasoning_history"), "expected a reasoning_history error, got: {err}");
    }

    #[test]
    fn default_max_tokens_is_quarter_context_clamped() {
        // Restores the bridge's per-provider output-cap fallback: ctx/4 clamped to [8k, 16384].
        assert_eq!(default_max_tokens(0), 8_000); // tiny context → floor
        assert_eq!(default_max_tokens(40_000), 10_000); // ctx/4 within range
        assert_eq!(default_max_tokens(1_000_000), 16_384); // huge context → ceiling
    }

    #[test]
    fn max_tokens_flows_from_provider_to_chat_options() {
        // A user-configured per-provider `max_tokens` must reach the per-call ChatOptions —
        // it was dropped in the native rewrite, capping replies at the adapter default.
        let mut pc = sample_provider();
        pc.max_tokens = Some(12_000);
        let lsp = atomcode_core::config::LspConfig::default();
        let shell =
            ShellConfig::from_provider(Some(&pc), &lsp, &ws_default(), std::path::Path::new("/w"), None, false, false);
        assert_eq!(shell.max_tokens, Some(12_000), "from_provider must carry provider.max_tokens");
        let cc = coding_config(&shell);
        assert_eq!(
            cc.chat_options.max_tokens,
            Some(12_000),
            "coding_config must thread it into ChatOptions"
        );
    }

    #[test]
    fn max_tokens_unset_leaves_chat_options_none() {
        // Unset ⇒ no per-call cap; the adapter's `default_max_tokens` fallback then applies.
        let pc = sample_provider(); // max_tokens: None
        let lsp = atomcode_core::config::LspConfig::default();
        let shell =
            ShellConfig::from_provider(Some(&pc), &lsp, &ws_default(), std::path::Path::new("/w"), None, false, false);
        assert_eq!(shell.max_tokens, None);
        assert_eq!(coding_config(&shell).chat_options.max_tokens, None);
    }

    fn sample_provider() -> atomcode_core::config::provider::ProviderConfig {
        atomcode_core::config::provider::ProviderConfig {
            provider_type: "claude".into(),
            api_key: Some("sk-1".into()),
            model: "m-1".into(),
            base_url: Some("https://api.example.com/v1".into()),
            system_prompt: None,
            user_agent: None,
            context_window: 64_000,
            max_tokens: None,
            thinking_type: Some("enabled".into()),
            thinking_keep: Some("all".into()),
            reasoning_history: Some("include".into()),
            reasoning_effort: Some("high".into()),
            thinking_enabled: Some(true),
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: true,
        }
    }

    #[test]
    fn from_provider_maps_present_provider() {
        let pc = sample_provider();
        let lsp = atomcode_core::config::LspConfig::default();
        let shell_cfg =
            ShellConfig::from_provider(Some(&pc), &lsp, &ws_default(), std::path::Path::new("/work"), None, true, false);
        assert_eq!(shell_cfg.api_key, "sk-1");
        assert_eq!(shell_cfg.base_url, "https://api.example.com/v1");
        assert_eq!(shell_cfg.model, "m-1");
        assert_eq!(shell_cfg.working_dir, std::path::PathBuf::from("/work"));
        assert_eq!(shell_cfg.context_window, 64_000);
        assert_eq!(shell_cfg.provider_type, "claude");
        assert_eq!(shell_cfg.reasoning_history.as_deref(), Some("include"));
        assert_eq!(shell_cfg.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(shell_cfg.thinking_enabled, Some(true));
        assert_eq!(shell_cfg.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(shell_cfg.thinking_keep.as_deref(), Some("all"));
        assert!(shell_cfg.mcp);
        assert!(shell_cfg.dangerously_skip_permissions);
        assert!(!shell_cfg.interactive);
    }

    #[test]
    fn from_provider_none_uses_neutral_defaults() {
        let lsp = atomcode_core::config::LspConfig::default();
        let shell_cfg = ShellConfig::from_provider(None, &lsp, &ws_default(), std::path::Path::new("/w"), None, false, true);
        assert_eq!(shell_cfg.api_key, "");
        assert_eq!(shell_cfg.base_url, "");
        assert_eq!(shell_cfg.model, "");
        assert_eq!(shell_cfg.context_window, 128_000);
        assert_eq!(shell_cfg.provider_type, "openai");
        assert_eq!(shell_cfg.reasoning_history, None);
        assert_eq!(shell_cfg.thinking_enabled, None);
        assert!(shell_cfg.mcp);
        assert!(!shell_cfg.dangerously_skip_permissions);
        assert!(shell_cfg.interactive);
    }
}
