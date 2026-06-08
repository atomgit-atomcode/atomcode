//! The assembly: wire L1 capabilities into a kernel [`Agent`] per the coding policy.

use crate::config::CodingAgentConfig;
use crate::discipline::VerifyCadenceHook;
use crate::persona::coding_persona;
use atomcode_capabilities::codeintel::{codeintel_tool_names, register_codeintel_tools};
use atomcode_capabilities::provider::{OpenAiCompatConfig, OpenAiCompatProvider};
use atomcode_capabilities::tools::{coding_tool_names, register_coding_tools, ApprovalMiddleware};
use atomcode_kernel::agent::Agent;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{MountedTools, ToolRegistry};
use std::sync::Arc;

/// Assemble a runnable, self-correcting coding agent from `cfg`.
///
/// Wires, all through existing kernel seams (no kernel change):
/// - **provider**: OpenAI-compatible adapter (L1) from the config's creds.
/// - **tools**: the neutral fs/bash toolset + codeintel (L1), all mounted.
/// - **approval**: an in-memory [`ApprovalMiddleware`] gate (L1) — registered FIRST, so a
///   later rewriting middleware can never change what the user approved.
/// - **persona**: the coding system prompt ([`coding_persona`]).
/// - **discipline**: the [`VerifyCadenceHook`] edit-then-verify loop.
/// - **liveness**: stream + request timeouts from the config (never unbounded).
///
/// Returns `Err` only if the provider fails to construct (e.g. a bad HTTP client config).
pub fn build_coding_agent(cfg: CodingAgentConfig) -> Result<Agent, String> {
    let mut provider_cfg = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
    provider_cfg.context_window = cfg.context_window;
    let provider =
        OpenAiCompatProvider::new(provider_cfg).map_err(|e| format!("provider init failed: {}", e.message))?;
    Ok(build_coding_agent_with(&cfg, Arc::new(provider)))
}

/// The SAME coding policy as [`build_coding_agent`] but with a CALLER-SUPPLIED provider
/// (a mock for tests, or any custom [`LlmProvider`]). Use this when you construct the
/// provider yourself; otherwise prefer [`build_coding_agent`].
pub fn build_coding_agent_with(cfg: &CodingAgentConfig, provider: Arc<dyn LlmProvider>) -> Agent {
    Agent::builder()
        .provider(provider)
        .tools(mount_coding_tools())
        .persona(coding_persona(&cfg.model))
        // Approval runs BEFORE any (future) arg-rewriting middleware — load-bearing order.
        .middleware(Arc::new(ApprovalMiddleware::in_memory()))
        .hook(Arc::new(VerifyCadenceHook::new()))
        .working_dir(cfg.working_dir.clone())
        .stream_timeout(cfg.stream_timeout)
        .request_timeout(cfg.request_timeout)
        .max_turn_end_continuations(cfg.max_turn_end_continuations)
        .build()
}

/// Register the neutral coding tools + codeintel into a fresh registry and mount the
/// union (everything visible to the model).
fn mount_coding_tools() -> MountedTools {
    let mut registry = ToolRegistry::new();
    register_coding_tools(&mut registry);
    register_codeintel_tools(&mut registry);
    let names: Vec<&str> =
        coding_tool_names().iter().chain(codeintel_tool_names().iter()).copied().collect();
    registry.mount(&names)
}
