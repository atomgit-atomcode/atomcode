pub mod context;
pub mod registry;
pub mod runner;
#[cfg(test)]
mod tests;
pub mod tools;
pub mod types;

pub use types::*;

// Re-export parallel_edit types used by the sub_agent dispatch layer.
// SubAgentTask and SubAgentPool are defined in agent::parallel_edit but
// referenced as sub_agent::SubAgentTask / sub_agent::SubAgentPool from
// tool::parallel_edit so they appear under a single namespace.
pub use super::parallel_edit::{SubAgentPool, SubAgentTask};
