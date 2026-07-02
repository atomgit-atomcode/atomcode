//! Kernel-native session agent for ACP sessions.
//!
//! Replicates the bridge's provider + config construction without depending on
//! `atomcode-bridge` (or `atomcode-core`). The single entry point [`spawn_session`]
//! runs the two-phase `prepare → assemble → spawn` pipeline and hands back a live
//! [`AgentHandle`] the session table (Task 6) can drive.

use std::path::PathBuf;
use std::sync::Arc;

use atomcode_coding::config::CodingAgentConfig;
use atomcode_coding::parts::{assemble, prepare, PrepareOptions};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::provider::LlmProvider;

/// Provider + model inputs for a single ACP session.
///
/// Constructed by the session dispatcher (Task 6) from the ACP `initialize`
/// handshake and the global provider configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    /// Provider adapter kind: `"openai"` (default), `"anthropic"`/`"claude"`,
    /// or `"ollama"`.  Empty / unknown → OpenAI-compatible.
    pub provider_type: String,
    /// Model context window in tokens.
    pub context_window: u32,
    /// Per-call output cap (`ChatOptions::max_tokens`).  `None` → provider default.
    pub max_tokens: Option<u32>,
}

impl EngineConfig {
    /// Build the `CodingAgentConfig` for this session's working directory.
    ///
    /// Sets only the fields that exist on this branch's `CodingAgentConfig`.
    /// `request_timeout` is cleared (`None`) so approval prompts park until the
    /// ACP client answers — the interactive contract, not the headless fail-closed one.
    pub fn to_coding_config(&self, cwd: PathBuf) -> CodingAgentConfig {
        let mut cfg = CodingAgentConfig::new(&self.api_key, &self.base_url, &self.model, cwd);
        cfg.context_window = self.context_window;
        cfg.chat_options.max_tokens = self.max_tokens;
        cfg.provider_type = self.provider_type.clone();
        // ACP sessions are long-lived and interactive: park on approval, not fail-closed.
        cfg.request_timeout = None;
        cfg
    }
}

/// Fallback output cap: `context_window / 4` clamped to `[8 000, 16 384]`.
///
/// Mirrors `atomcode-bridge`'s `default_max_tokens` so the per-provider fallback is
/// proportionate for large windows without overflowing the API's hard limit.
fn default_max_tokens(context_window: u32) -> u32 {
    (context_window / 4).clamp(8_000, 16_384)
}

/// Returns `true` if `base_url` points at the AtomGit AI gateway.
///
/// Used as a fail-fast guard inside `build_provider`: the fallback OpenAI-compat
/// path cannot produce an authenticated (signed) request for the AtomGit gateway,
/// so we bail out clearly instead of generating silent 401s.
fn is_atomgit_gateway(base_url: &str) -> bool {
    let lower = base_url.to_ascii_lowercase();
    lower.contains("atomgit.com") || lower.contains("api-ai.gitcode.com")
}

/// Build a provider adapter for the given config.
///
/// Mirrors `atomcode-bridge::runtime::build_provider` but without the AtomGit
/// gateway signer — `atomcode-acp` does not depend on `atomcode-core`, and ACP
/// sessions must use an externally-issued `api_key` rather than a signed gateway.
///
/// The three branches (anthropic / ollama / openai-compat) are kept in sync with
/// the bridge; field assignments use the exact same real config structs.
pub fn build_provider(cfg: &CodingAgentConfig) -> anyhow::Result<Arc<dyn LlmProvider>> {
    use atomcode_capabilities::provider::{
        AnthropicConfig, AnthropicProvider, OllamaConfig, OllamaProvider, OpenAiCompatConfig,
        OpenAiCompatProvider, ReasoningPolicy,
    };

    match cfg.provider_type.as_str() {
        "claude" | "anthropic" => {
            let mut ac = AnthropicConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
            ac.context_window = cfg.context_window;
            ac.max_tokens = default_max_tokens(cfg.context_window);
            ac.thinking = cfg.thinking_enabled.unwrap_or(false);
            Ok(Arc::new(
                AnthropicProvider::new(ac).map_err(|e| anyhow::anyhow!(e.message))?,
            ))
        }
        "ollama" => {
            let mut oc = OllamaConfig::new(&cfg.base_url, &cfg.model);
            oc.api_key = cfg.api_key.clone();
            oc.context_window = cfg.context_window;
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
            pc.max_tokens = Some(default_max_tokens(cfg.context_window));
            // Honor `reasoning_history` override; unset → None → adapter auto-detects.
            // A typo fails fast (parity with the legacy engine and the bridge).
            pc.reasoning_policy =
                ReasoningPolicy::from_config(cfg.reasoning_history.as_deref())
                    .map_err(|e| anyhow::anyhow!(e))?;
            pc.thinking_type = cfg.thinking_type.clone();
            pc.thinking_keep = cfg.thinking_keep.clone();
            if is_atomgit_gateway(&cfg.base_url) {
                anyhow::bail!(
                    "ACP engine cannot build an authenticated provider for the AtomGit gateway ({}); \
                     the CLI must supply a pre-built (signed) provider",
                    cfg.base_url
                );
            }
            Ok(Arc::new(
                OpenAiCompatProvider::new(pc).map_err(|e| anyhow::anyhow!(e.message))?,
            ))
        }
    }
}

/// Spawn a kernel-native agent for a new ACP session.
///
/// Runs the two-phase `prepare → assemble → spawn` pipeline and returns a live
/// [`AgentHandle`] the session dispatcher (Task 6) can drive.
///
/// `provider` — when `Some`, the pre-built (authenticated) provider is used
/// directly; when `None`, [`build_provider`] constructs a fallback from the
/// engine config (valid for non-gateway endpoints only).
pub async fn spawn_session(
    engine: &EngineConfig,
    cwd: PathBuf,
    provider: Option<Arc<dyn LlmProvider>>,
) -> anyhow::Result<AgentHandle> {
    let cfg = engine.to_coding_config(cwd);
    // Production-parity defaults: MCP, memory, web, review, fresh session.
    let mut parts = prepare(&cfg, PrepareOptions::default())
        .await
        .map_err(|e| anyhow::anyhow!("acp prepare failed: {e}"))?;
    let provider = match provider {
        Some(p) => p,
        None => build_provider(&cfg)?,
    };
    let agent = assemble(&mut parts, &cfg, provider)
        .map_err(|e| anyhow::anyhow!("acp assemble failed: {e}"))?;
    Ok(agent.spawn())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_config_builds_coding_config() {
        let e = EngineConfig {
            api_key: "k".into(),
            base_url: "https://x".into(),
            model: "m".into(),
            provider_type: "openai".into(),
            context_window: 200_000,
            max_tokens: Some(8192),
        };
        let cfg = e.to_coding_config(std::path::PathBuf::from("/tmp/work"));
        assert_eq!(cfg.model, "m");
        assert_eq!(cfg.context_window, 200_000);
        assert_eq!(cfg.provider_type, "openai");
        assert_eq!(cfg.working_dir, std::path::PathBuf::from("/tmp/work"));
        assert_eq!(cfg.chat_options.max_tokens, Some(8192));
    }
}
