//! What is left of the product's own team runner (`docs/tui-replaces-tuix-plan.md` M6.5).
//!
//! Running a team is the harness's job now — `plugins/team.rs`, mounted by
//! `on_harness.rs` as `team-in-process`. Two of the three files here had no
//! production caller left and are gone: the tool (`TeamTool`, never
//! constructed outside its own tests) and the runner factory
//! (`TeamRunnerFactory`, whose only production mention was
//! `parts.rs` writing `None` into a field nobody read).
//!
//! `manager.rs` stays, and only half of it is alive: the relay that turns the
//! harness's progress into `CodingRuntimeEvent::Team` for the one front end
//! that draws a team panel. The other half — the run store behind `delegate` /
//! `wait` / `stop` / `snapshot` — is unreachable too (nothing calls `delegate`,
//! so the store is always empty), **but it cannot be cut on its own**:
//! `stop_all` is still called from three places and reads that store, so
//! removing it means unpicking the `team_manager` parameter from
//! `quiesce_current_agent` and `stop_current_agent` as well. That belongs with
//! 6.4, when the front end this whole module feeds goes — cutting it earlier
//! buys ~400 lines and touches the turn loop to get them.

mod manager;

pub use manager::{
    GenerationTeamEvent, TeamActivitySink, TeamJobFactory, TeamMemberOutcome, TeamMemberSnapshot,
    TeamMemberStatus, TeamModelFactory, TeamRunManager, TeamRunSnapshot, TeamRuntimeConfig,
    TeamSnapshot, TeamWaitOutcome,
};
