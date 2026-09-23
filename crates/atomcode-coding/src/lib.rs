//! # atomcode-coding (L2)
//!
//! The CODING specialization. It assembles the neutral kernel ([`atomcode_kernel`]),
//! the capabilities ([`atomcode_capabilities`]) and the plugin harness
//! ([`atomcode_harness`]) into a runnable coding agent that **self-corrects** — and
//! it does so with ZERO `atomcode-core` involvement.
//!
//! Two phases, both public:
//!
//! 1. [`prepare`] builds the capability graph — tools, skills, MCP, the session
//!    binding, the approval stores — and hands back [`CodingParts`].
//! 2. [`runtime::mount`] mounts that graph as a plexus tree and returns the
//!    [`AgentHandle`](atomcode_kernel::agent::AgentHandle) a driver speaks to. The
//!    product IS the row list in [`on_harness`] plus the host rows in
//!    [`host_rows`]; there is no second assembly.
//!
//! What this crate owns on top of the neutral parts:
//! - **the row list** — which rows this product mounts, and the host rows only it
//!   can build (its session store, its delegation tools, its rate-limit policy);
//! - **the persona** — [`persona::coding_persona`], the coding system prompt;
//! - **the discipline** — [`discipline::unverified_edit`], the edit-then-verify
//!   judgement the `verify-cadence` row acts on.
//!
//! ```no_run
//! # async fn demo() -> Result<(), String> {
//! use atomcode_coding::{prepare, CodingAgentConfig, PrepareOptions};
//!
//! let cfg = CodingAgentConfig::new("sk-...", "https://api.deepseek.com/v1", "deepseek-chat", ".");
//! let opts = PrepareOptions::default();
//! let parts = prepare(&cfg, opts.clone()).await.map_err(|e| e.to_string())?;
//! # let provider: std::sync::Arc<dyn atomcode_kernel::provider::LlmProvider> = todo!();
//! let mounted = atomcode_coding::runtime::mount(&parts, &cfg, &opts, provider).await?;
//! // `mounted.handle` drives turns; `mounted.app` must outlive it.
//! # Ok(()) }
//! ```
//!
//! For the whole product with its session lifecycle, `/model`, undo and the rest,
//! use [`runtime::CodingRuntime`] rather than mounting by hand.

// Redirect ATOMCODE_HOME to a throwaway temp dir before any unit test runs, so the
// suite can't persist into the developer's real ~/.atomcode (see
// atomcode_kernel::test_support).
#[cfg(test)]
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

pub mod config;
mod controllers;
pub mod discipline;
pub mod front_end;
pub mod on_harness;
pub mod parts;
pub mod persona;
pub mod plan_mode;
pub mod plugin_hooks;
pub mod policy_rows;
pub mod provider_factory;
pub mod runtime;
pub mod session_store;
pub mod session_title;
pub mod team;
mod team_progress;
pub mod telemetry;
pub mod vision;

mod execution_policy;
pub mod host_rows;
mod init_prompt;
mod mcp_instructions;
pub mod native_log;
mod next_prompt_suggestion;
mod rate_limit;
mod skill_first;
pub mod subagent_tiers;
mod todo;
mod tool_intent;

/// The image type carried by [`UserInput`] / [`ImagePreprocessor`], re-exported
/// so driver crates can implement the hook without naming `atomcode_kernel`.
pub use atomcode_kernel::message::ImageContent;
pub use config::{
    apply_provider_config, resolve_loop_max_rounds, resolve_turn_max_rounds, CodingAgentConfig,
    CodingRuntimeConfig, SubagentModelProviders, SubagentModelResolver, SubagentProvider,
    TierProvider,
};
pub use controllers::{GoalPhase, GoalProgress, GoalTerminal, LoopProgress};
pub use init_prompt::{build_init_prompt, INIT_PROMPT, INIT_PROMPT_ZH_CN};
pub use parts::{
    prepare, prepare_from_config, prepare_with_plugin_hook_source, prepare_with_plugin_hooks,
    subagent_enabled_from_env, CodingParts, McpRowFacts, PrepareOptions, SessionBinding,
    SessionMode, SubagentPolicy,
};
pub use persona::coding_persona;
pub use plan_mode::PlanModeGate;
pub use plugin_hooks::{PluginHookSource, StaticPluginHookSource};
pub use provider_factory::{
    atomgit_provider_factory, derive_tier_config, install_subagent_tiers, refresh_subagent_tiers,
    resolve_subagent_tier_thunks, tier_provider_builder, AtomGitProviderAuthenticator,
    CodingProviderFactory, DefaultCodingProviderFactory, ProviderAuthenticator, ProviderBuildError,
};
pub use rate_limit::{
    AccountUsage, DayUse, Entitlement, ModelSeries, ModelUse, RateLimitWindow,
    RateLimitWindowSource,
};
pub use runtime::{
    CodingRuntime, CodingRuntimeEvent, CodingRuntimeEvents, CodingRuntimeHandle,
    CodingRuntimeStart, DeferredRuntimeState, DriverCommand, ImagePreprocessor, LocalContextInput,
    McpDetailSnapshot, McpRowsSnapshot, McpStatusSnapshot, McpToolsSnapshot, ProviderBootstrap,
    ProviderUnavailableReason, ReconfigureKind, RewindCatalog, RewindResult, RewindScope,
    RuntimeContextStats, RuntimeError, RuntimeExit, RuntimeExitReason, RuntimeGeneration,
    RuntimeMode, RuntimePhase, RuntimeRequest, RuntimeSessionInfo, RuntimeSnapshotError,
    RuntimeStartError, RuntimeStatus, RuntimeTurnStats, RuntimeUnavailable, SequencedRuntimeEvent,
    SessionChanged, SubmitReceipt, TurnCompletion, UndoResult, UserInput, VisionNotice,
    WorkspaceScope,
};
pub use telemetry::{TelemetryHook, ToolTelemetryMiddleware};
pub use todo::TodoHook;
pub use vision::{run_vl_caption, should_skip, vl_model_display, PreprocessOutcome};

/// Re-export the CC external-hooks types so host adapters that resolve
/// plugin-contributed hooks can name [`cc_hooks::HookConfig`] without a direct
/// `atomcode-capabilities` dependency or its feature flag.
pub use atomcode_capabilities::cc_hooks;
