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
pub mod discipline;
pub mod parts;
pub mod persona;
pub mod plan_mode;
pub mod runtime;
pub mod telemetry;

mod assemble;
mod init_prompt;
mod rate_limit;
mod todo;
pub mod subagent_tiers;

pub use assemble::{build_coding_agent, build_coding_agent_with};
pub use config::{CodingAgentConfig, SubagentProvider, TierProvider};
pub use discipline::VerifyCadenceHook;
pub use plan_mode::PlanModeGate;
pub use runtime::{CodingRuntimeHandle, RuntimeUnavailable};
pub use todo::TodoHook;
pub use telemetry::{TelemetryHook, ToolTelemetryMiddleware};
pub use parts::{
    assemble, prepare, prepare_with_plugin_hooks, CodingParts, PrepareOptions, SessionBinding,
    SessionMode, subagent_enabled_from_env,
};
pub use persona::coding_persona;
pub use init_prompt::INIT_PROMPT;

/// Re-export the CC external-hooks types so drivers (e.g. the bridge) that resolve
/// plugin-contributed hooks can name [`cc_hooks::HookConfig`] without a direct
/// `atomcode-capabilities` dependency or its feature flag.
pub use atomcode_capabilities::cc_hooks;
