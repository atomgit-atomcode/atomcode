//! The assembly: wire L1 capabilities into a kernel [`Agent`] per the coding policy.

use crate::config::CodingAgentConfig;
use crate::discipline::VerifyCadenceHook;
use crate::persona::coding_persona;
use atomcode_capabilities::codeintel::{codeintel_tool_names, register_codeintel_tools};
use atomcode_capabilities::provider::{OpenAiCompatConfig, OpenAiCompatProvider};
use atomcode_capabilities::session::SessionContextHook;
use atomcode_capabilities::tools::{
    coding_tool_names, register_coding_tools_with_vision, ApprovalMiddleware, OpenFileWorkspaceGate,
    WriteApprovalGate,
};
use atomcode_core::provider::model_name_suggests_vision;
use atomcode_kernel::agent::Agent;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{MountedTools, ToolRegistry};
use std::sync::Arc;

/// Assemble a runnable, self-correcting coding agent from `cfg` — the MINIMAL sync
/// path (tools + codeintel only). For the FULL agent (web / skills / mcp / session
/// persistence / memory) use the two-phase [`crate::prepare`] → [`crate::assemble`].
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
    let summary_provider = provider.clone(); // tier-2 overflow summary uses the same provider
    let mut builder = Agent::builder()
        .provider(provider)
        .tools(mount_coding_tools(model_name_suggests_vision(&cfg.model)))
        .persona(coding_persona(&cfg.model))
        // Auto-approve in-workspace open_file (it's Risky → would otherwise prompt on every
        // preview). This path pins an immutable working_dir, so the gate pins the same root.
        // BEFORE approval so its `Allow` short-circuits the prompt.
        .middleware(Arc::new(OpenFileWorkspaceGate::pinned(cfg.working_dir.clone())))
        // Workspace-aware, per-path approval for the file-mutation tools (v1 granularity):
        // in-workspace non-sensitive writes auto-approve, sensitive writes always re-prompt,
        // out-of-workspace writes prompt with a per-path "Always". BEFORE the generic approval
        // gate so its `Allow` short-circuits the prompt. Pins the same immutable root.
        .middleware(Arc::new(WriteApprovalGate::pinned(cfg.working_dir.clone())))
        // Approval runs BEFORE any (future) arg-rewriting middleware — load-bearing order.
        .middleware(Arc::new(ApprovalMiddleware::in_memory()))
        // Env / project-instructions / git context at session start (after persona).
        .hook(Arc::new(SessionContextHook::new(cfg.working_dir.clone())))
        .hook(Arc::new(VerifyCadenceHook::new()))
        .working_dir(cfg.working_dir.clone())
        // Cache-friendly task-boundary stub + hard-overflow recovery ladder (stub→truncate
        // →drain+LLM-summary). The overflow path is off the normal path (typed error only).
        .compaction(Arc::new(atomcode_capabilities::compaction::OverflowCompaction::new(
            atomcode_capabilities::compaction::StubCompaction::default(),
            Some(summary_provider),
        )))
        .compact_threshold(cfg.compact_threshold)
        .stream_timeout(cfg.stream_timeout)
        .max_continuations(cfg.max_continuations)
        // Ctrl-C semantics: false = UNDO (default), true = PRESERVE the interrupted turn.
        .keep_interrupted_context(cfg.keep_interrupted_context);
    // Approval liveness: `Some(d)` ⇒ fail-closed after `d` (headless); `None` ⇒ PARK until
    // answered (interactive). Kernel defaults to unbounded when unset, so None = park.
    if let Some(d) = cfg.request_timeout {
        builder = builder.request_timeout(d);
    }
    builder.build()
}

/// Register the neutral coding tools + codeintel into a fresh registry and mount the
/// union (everything visible to the model).
fn mount_coding_tools(vision: bool) -> MountedTools {
    let mut registry = ToolRegistry::new();
    register_coding_tools_with_vision(&mut registry, vision);
    register_codeintel_tools(&mut registry);
    let names: Vec<&str> =
        coding_tool_names().iter().chain(codeintel_tool_names().iter()).copied().collect();
    registry.mount(&names)
}
