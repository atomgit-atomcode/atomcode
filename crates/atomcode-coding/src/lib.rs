//! # atomcode-coding (L2)
//!
//! The CODING specialization. It assembles the neutral kernel ([`atomcode_kernel`]) +
//! capabilities ([`atomcode_capabilities`]) into a runnable coding agent that
//! **self-corrects** — and it does so with ZERO `atomcode-core` involvement.
//!
//! NOTE: [`build_coding_agent`] is the MINIMAL sync assembly (tools + codeintel
//! only). The FULL agent — web/skills/mcp/session persistence/memory wired — is the
//! two-phase [`prepare`] → [`assemble`] in [`parts`].
//!
//! L2 owns three things, all mounted via existing kernel seams (no new kernel surface):
//! 1. **Assembly** — [`build_coding_agent`]: wires provider + tools + codeintel +
//!    approval + persona + the verify discipline into a kernel [`Agent`](atomcode_kernel::agent::Agent).
//! 2. **Persona** — [`persona::coding_persona`]: the coding system prompt.
//! 3. **Discipline** — [`discipline::VerifyCadenceHook`]: an edit-then-verify
//!    `offer_continuation` hook (the coding self-correction loop).
//!
//! ```no_run
//! # async fn demo() -> Result<(), String> {
//! use atomcode_coding::{build_coding_agent, CodingAgentConfig};
//! use atomcode_kernel::agent::AutoRespond;
//!
//! let agent = build_coding_agent(CodingAgentConfig::new(
//!     "sk-...", "https://api.deepseek.com/v1", "deepseek-chat", ".",
//! ))?;
//! let outcome = agent.run_to_completion("fix the build", AutoRespond::AllowAll).await;
//! println!("{}", outcome.text);
//! # Ok(()) }
//! ```

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
pub mod parts;
pub mod persona;
pub mod plan_mode;
pub mod plugin_hooks;
pub mod provider_factory;
pub mod runtime;
pub mod session_title;
pub mod telemetry;
pub mod vision;

mod assemble;
mod init_prompt;
mod rate_limit;
mod skill_first;
pub mod subagent_tiers;
mod todo;

pub use assemble::{build_coding_agent, build_coding_agent_with};
/// The image type carried by [`UserInput`] / [`ImagePreprocessor`], re-exported
/// so driver crates can implement the hook without naming `atomcode_kernel`.
pub use atomcode_kernel::message::ImageContent;
pub use config::{
    apply_provider_config, resolve_loop_max_rounds, resolve_turn_max_rounds, CodingAgentConfig,
    CodingRuntimeConfig, SubagentProvider, TierProvider,
};
pub use controllers::{GoalProgress, LoopProgress};
pub use discipline::VerifyCadenceHook;
pub use init_prompt::INIT_PROMPT;
pub use parts::{
    assemble, prepare, prepare_with_plugin_hook_source, prepare_with_plugin_hooks,
    subagent_enabled_from_env, CodingParts, PrepareOptions, SessionBinding, SessionMode,
};
pub use persona::{coding_persona, coding_persona_with_language, commit_language_guidance};
pub use plan_mode::PlanModeGate;
pub use plugin_hooks::{PluginHookSource, StaticPluginHookSource};
pub use provider_factory::{
    atomgit_provider_factory, derive_tier_config, install_subagent_tiers, refresh_subagent_tiers,
    resolve_subagent_tier_thunks, tier_provider_builder, AtomGitProviderAuthenticator,
    CodingProviderFactory, DefaultCodingProviderFactory, ProviderAuthenticator, ProviderBuildError,
};
pub use rate_limit::{RateLimitWindow, RateLimitWindowSource};
pub use vision::{run_vl_caption, should_skip, vl_model_display, PreprocessOutcome};
pub use runtime::{
    CodingRuntime, CodingRuntimeEvent, CodingRuntimeEvents, CodingRuntimeHandle,
    CodingRuntimeStart, DeferredRuntimeState, DriverCommand, ImagePreprocessor, LocalContextInput,
    McpStatusSnapshot, McpToolsSnapshot, ProviderBootstrap, ProviderUnavailableReason,
    ReconfigureKind, ReprepareInput, RuntimeContextStats, RuntimeError, RuntimeExit,
    RuntimeExitReason, RuntimeGeneration, RuntimeMode, RuntimePhase, RuntimeRequest,
    RuntimeSessionInfo, RuntimeSnapshotError, RuntimeStartError, RuntimeStatus, RuntimeTurnStats,
    RuntimeUnavailable, SequencedRuntimeEvent, SessionChanged, SubmitReceipt, TurnCompletion,
    UndoResult, UserInput, VisionNotice,
};
pub use telemetry::{TelemetryHook, ToolTelemetryMiddleware};
pub use todo::TodoHook;

/// Re-export the CC external-hooks types so host adapters that resolve
/// plugin-contributed hooks can name [`cc_hooks::HookConfig`] without a direct
/// `atomcode-capabilities` dependency or its feature flag.
pub use atomcode_capabilities::cc_hooks;
