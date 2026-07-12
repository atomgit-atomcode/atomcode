//! Kernel-native runtime spawn helper for the daemon.
//!
//! Currently **unused** — scaffolded behind the `ATOMCODE_DAEMON_ENGINE`
//! runtime switch so later tasks can wire it in without touching this module.
//!
//! The spawn pipeline (`prepare → assemble → spawn`) mirrors the working
//! template in `crates/atomcode-cli/src/acp/engine.rs::spawn_session`.

use std::sync::Arc;

use atomcode_bridge::BridgeConfig;
use atomcode_coding::config::CodingAgentConfig;
use atomcode_coding::parts::{assemble, prepare, PrepareOptions};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::provider::LlmProvider;

/// Map a [`BridgeConfig`] to a [`CodingAgentConfig`] for the kernel-native path.
///
/// Mirrors the field-by-field construction in `atomcode_bridge::runtime::Bridge::run`
/// (lines 286-342) so the kernel path honors exactly the same knobs as the bridge path.
/// Fields not present in `BridgeConfig` are left at `CodingAgentConfig::new()`'s defaults
/// (the same fields bridge also leaves at their defaults).
///
/// The subagent tier providers (`subagent_fast_provider` / `subagent_capable_provider`) are
/// NOT wired here: they require loading the full `atomcode_config::Config` and calling
/// `resolve_tier_thunks`, which is a bridge-internal function. A future task will wire those
/// when the kernel path grows full subagent support.
#[allow(dead_code)]
pub fn coding_config_from_bridge(cfg: &BridgeConfig) -> CodingAgentConfig {
    let mut coding_cfg = CodingAgentConfig::new(
        &cfg.api_key,
        &cfg.base_url,
        &cfg.model,
        &cfg.working_dir,
    );
    coding_cfg.context_window = cfg.context_window;
    // User-configured per-call output cap (parity with `apply_reload_provider`); `None` ⇒
    // the per-provider fallback in `build_provider` applies.
    coding_cfg.chat_options.max_tokens = cfg.max_tokens;
    coding_cfg.telemetry = cfg.telemetry.clone();
    coding_cfg.reasoning_history = cfg.reasoning_history.clone();
    // `/effort`: thread the per-provider reasoning_effort into the per-call ChatOptions
    // so the kernel path actually emits it (openai_compat → `reasoning_effort` body field).
    coding_cfg.chat_options.reasoning_effort =
        atomcode_kernel::provider::ReasoningEffort::from_config(cfg.reasoning_effort.as_deref());
    // Adapter selection + thinking controls (so Claude-/Ollama-native + /think work).
    coding_cfg.provider_type = cfg.provider_type.clone();
    coding_cfg.thinking_enabled = cfg.thinking_enabled;
    coding_cfg.thinking_type = cfg.thinking_type.clone();
    coding_cfg.thinking_keep = cfg.thinking_keep.clone();
    // Gateway identity: product UA + TLS-verify toggle.
    coding_cfg.user_agent = cfg.user_agent.clone();
    coding_cfg.skip_tls_verify = cfg.skip_tls_verify;
    coding_cfg.loop_max_rounds = cfg.loop_max_rounds;
    // Interactive drivers PARK approvals (a present human must not be auto-denied for
    // thinking too long); headless keeps the configured fail-closed timeout.
    if cfg.interactive {
        coding_cfg.request_timeout = None;
    }
    coding_cfg.keep_interrupted_context = cfg.keep_interrupted_context;
    coding_cfg
}

/// Returns `true` when `ATOMCODE_DAEMON_ENGINE=kernel` is set in the environment.
///
/// Used as a feature gate: future tasks branch on this to decide whether to
/// drive turns through the kernel-native path or the legacy bridge.
#[allow(dead_code)]
pub fn engine_is_kernel() -> bool {
    std::env::var("ATOMCODE_DAEMON_ENGINE").as_deref() == Ok("kernel")
}

/// Spawn a kernel-native agent for a daemon turn.
///
/// Runs the two-phase `prepare → assemble → spawn` pipeline and returns a live
/// [`AgentHandle`] that a future task's turn executor can drive.
///
/// # Arguments
/// * `cfg` — coding agent configuration (working dir, model, provider, etc.).
/// * `provider` — pre-built (possibly authenticated) LLM provider.
/// * `opts` — prepare options controlling which hooks are loaded.
#[allow(dead_code)]
pub async fn spawn(
    cfg: &CodingAgentConfig,
    provider: Arc<dyn LlmProvider>,
    opts: PrepareOptions,
) -> anyhow::Result<AgentHandle> {
    let mut parts = prepare(cfg, opts).await?;
    let agent = assemble(&mut parts, cfg, provider)
        .map_err(|e| anyhow::anyhow!("daemon kernel assemble failed: {e}"))?;
    Ok(agent.spawn())
}
