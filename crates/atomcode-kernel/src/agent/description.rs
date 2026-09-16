//! What a front end is told about an agent: who it is, what it runs on, and
//! whether it is busy (`docs/adr/0022` §5).
//!
//! Pushed, never asked for. A subscriber hears an agent described when it
//! subscribes, hears its members come and go, and hears its status change —
//! so a screen can draw a status bar and a member list without reading any
//! service of the agent's own.

use serde::{Deserialize, Serialize};

use crate::provider::ReasoningEffort;

/// Where an agent stands. Changes often, so it travels on its own event rather
/// than inside the description.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    /// Nothing owed; waiting for input.
    Idle,
    /// A turn is open.
    Working,
    /// Cancellation asked for; the current step is finishing.
    Stopping,
}

/// A team member's identity: the name the lead gave it and the role it was
/// given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberIdentity {
    pub name: String,
    pub role: String,
}

/// One agent, as a front end needs to know it.
///
/// Every field is filled by whatever implements it — the model by the model
/// the agent's realm resolves, a role's name and thinking level by the row that
/// gave the role — so a deployment that swaps a row changes the description
/// with it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentDescription {
    /// The session this agent writes. What every other event and command
    /// addresses it by.
    pub session: String,
    /// The session that delegated to this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Set for a team member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<MemberIdentity>,
    /// The model id the agent's requests go to, when one is mounted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether an image attached to a message reaches the model. A front end
    /// asks before it attaches one.
    #[serde(default)]
    pub supports_vision: bool,
    /// The thinking level the agent's requests carry. `None` is no opinion:
    /// the endpoint's own default stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Whether the agent can compact its conversation.
    #[serde(default)]
    pub compaction: bool,
    /// The commands a person can run against this agent
    /// ([`super::CommandDescription`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<super::CommandDescription>,
}
