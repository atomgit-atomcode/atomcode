//! # atomcode-harness — AtomCode's coding agent, reassembled as a plugin tree
//!
//! The same capabilities the shipped agent uses (the OpenAI-compatible adapter,
//! the real fs and bash tools, argument repair) mounted on
//! [`atomcode_plexus`] instead of a builder chain. The difference is not what it
//! can do — it is what a user can change without a fork:
//!
//! | in `atomcode-coding` | here |
//! |---|---|
//! | `Agent::builder().provider(..).tools(..).middleware(..)` in `assemble.rs` | rows in a config tree |
//! | the turn loop is `atomcode_kernel::agent` | the turn loop is the `agent-loop` row |
//! | approval is a `ToolMiddleware` compiled into the assembly | a listener on `tools/execute`, mounted by a row |
//! | the persona is a `String` the assembly passes in | a fragment contributed to the `system-prompt` registry |
//! | swapping the provider means editing `assemble.rs` | `[[patch]] id = "llm"` |
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use atomcode_harness::{bundle, plugins};
//! use atomcode_plexus::App;
//!
//! let mut app = App::new(plugins::catalog(), bundle::tree(&[])?);
//! app.start().await?;
//! let outcome = atomcode_harness::run_turn(&app, "list the files here").await?;
//! println!("{}", outcome.text);
//! # Ok(()) }
//! ```

pub mod agent;
pub mod bundle;
pub mod control;
pub mod events;
pub mod exec;
pub mod plugins;
pub mod profile;
pub mod seam_map;
pub mod seams;
pub mod session;
pub mod world;

use std::path::PathBuf;

/// Where the harness keeps its own state (`$ATOMCODE_HOME`, else `~/.atomcode`).
///
/// One resolver, so every persisting plugin agrees on the root and a test can
/// redirect all of them at once.
pub fn home() -> PathBuf {
    if let Ok(dir) = std::env::var("ATOMCODE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".atomcode"))
        .unwrap_or_else(|_| PathBuf::from(".atomcode"))
}

use std::sync::Arc;

use atomcode_plexus::{App, PlexusError};

use crate::agent::Agent;
use crate::events::AgentInfo;
use crate::seams::{AgentLoopSvc, AgentsSvc, TurnOutcome};

/// Create an agent in the running tree.
///
/// It gets a realm of its own, so anything registered through `agent.ctx()` is
/// scoped to it. Announced on `agent/created` so a UI or supervisor can attach
/// without being told.
pub fn create_agent(app: &App) -> Result<Arc<Agent>, PlexusError> {
    let ctx = app.context();
    let agent = ctx.require::<AgentsSvc>()?.create(&ctx);
    ctx.emit::<crate::events::AgentCreated>(&AgentInfo { id: agent.id() });
    Ok(agent)
}

/// Drive one turn for an existing agent, whose inbox already has work.
pub async fn drive(app: &App, agent: &Agent) -> Result<TurnOutcome, PlexusError> {
    let driver = app.context().require::<AgentLoopSvc>()?;
    Ok(driver.drive(agent).await)
}

/// The one-shot convenience: an agent, one message, one turn.
///
/// The caller names no implementation — which is the point. A profile that
/// mounts a different driver changes what this does with no change here.
pub async fn run_turn(app: &App, prompt: &str) -> Result<TurnOutcome, PlexusError> {
    let agent = create_agent(app)?;
    agent.send(prompt);
    drive(app, &agent).await
}
