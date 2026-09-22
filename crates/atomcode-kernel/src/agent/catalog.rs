//! The commands a person can run against an agent, beyond talking to it
//! (`docs/adr/0021` §10).
//!
//! A capability row registers its commands when it mounts and they go when it
//! unmounts; the catalog reaches a front end inside [`super::AgentDescription`],
//! and every entry is run the same way, by
//! [`crate::event::AgentCommand::Invoke`]. So a new capability brings its
//! commands with it, and the contract does not grow a type per command.
//!
//! These are a person's commands, run from a front end. They are not tools: a
//! model never sees them.

use serde::{Deserialize, Serialize};

/// One command in the catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDescription {
    /// What a person types, without any prefix a front end puts before it.
    pub name: String,
    /// How the arguments are written, for a hint beside the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    /// What it does, in a line.
    pub summary: String,
    pub target: CommandTarget,
}

/// What a command acts on, which is what [`crate::event::AgentCommand::Invoke`]
/// addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandTarget {
    /// The session as a whole.
    Session,
    /// One agent in it — a member, say.
    Agent,
}
