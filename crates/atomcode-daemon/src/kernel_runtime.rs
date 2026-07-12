//! Kernel-native runtime spawn helper for the daemon.
//!
//! Currently **unused** — scaffolded behind the `ATOMCODE_DAEMON_ENGINE`
//! runtime switch so later tasks can wire it in without touching this module.
//!
//! The spawn pipeline (`prepare → assemble → spawn`) mirrors the working
//! template in `crates/atomcode-cli/src/acp/engine.rs::spawn_session`.

use std::sync::Arc;

use atomcode_coding::config::CodingAgentConfig;
use atomcode_coding::parts::{assemble, prepare, PrepareOptions};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::provider::LlmProvider;

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
