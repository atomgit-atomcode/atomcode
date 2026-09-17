//! Host control: what a front end asks of whatever hosts its agents
//! (`docs/adr/0021` §2).
//!
//! The handle protocol ([`crate::event`]) speaks to one agent about its own
//! conversation. What is here is above any one agent: which session is live, and
//! settings a person expects to outlive the agent that carries them today. A
//! front end reaches it through [`HostControl`] and never learns who the host is
//! — a coding runtime, a daemon, a test.
//!
//! Three rules shape every item:
//!
//! - **The payload is an intent, not an implementation.** "Resume that session",
//!   never a configuration or a conversation to rebuild one from.
//! - **No session model leaks through.** Nothing here says how a host replaces a
//!   session. A front end sees a session's identity change, and that is all.
//! - **Commands are addressed** (`docs/adr/0021` §9). One that acts on the live
//!   session names the session the caller is looking at; if the host has moved
//!   on to another, the command is refused rather than applied to a session the
//!   caller never saw.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::provider::ReasoningEffort;
use crate::session::SeqNo;

/// What a front end asks the host to do.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostCommand {
    /// Put an empty session in place of `session`, in the same working
    /// directory.
    NewSession { session: String },
    /// Put the stored session `target` in place of `session`.
    Resume { session: String, target: String },
    /// The thinking level `session`'s requests carry from now on. `None` is no
    /// opinion: the endpoint's own default stands.
    SetReasoningEffort {
        session: String,
        level: Option<ReasoningEffort>,
    },
    /// The stored sessions a person could resume, newest first — those of one
    /// working directory, or all of them.
    ListSessions { working_dir: Option<String> },
}

impl HostCommand {
    /// The live session this command acts on, if it acts on one.
    pub fn addressed(&self) -> Option<&str> {
        match self {
            Self::NewSession { session }
            | Self::Resume { session, .. }
            | Self::SetReasoningEffort { session, .. } => Some(session),
            Self::ListSessions { .. } => None,
        }
    }
}

/// What a command that went through produced.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostReply {
    /// Done. The live session is the one it was.
    Done,
    /// The live session is now `session`. A front end drops the stream of the
    /// one it replaced and follows this one (`docs/adr/0022` §6).
    SessionChanged {
        session: String,
    },
    Sessions {
        sessions: Vec<StoredSession>,
    },
}

/// One stored session, as a picker shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Unix milliseconds.
    #[serde(default)]
    pub created_at: u64,
    /// Unix milliseconds.
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub turns: u32,
    /// Written by a newer build than the host's: listed so a person knows it is
    /// there, refused if they try to resume it (`docs/adr/0024` §16).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_newer_version: bool,
}

/// Something that happened on the host, whoever caused it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostEvent {
    /// The live session was replaced — at this front end's request, another's,
    /// or the host's own.
    SessionChanged {
        session: String,
        previous: Option<String>,
    },
}

/// Why a host refused or failed a command (`docs/adr/0021` §8).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostError {
    /// Not now: something the host is doing has to finish first.
    Busy {
        reason: String,
    },
    /// Stopped before it took effect.
    Cancelled,
    /// Another host holds that session.
    SessionInUse {
        id: String,
    },
    /// The host cannot take commands now, or at all any more.
    Unavailable,
    /// There is no model to talk to.
    ProviderUnavailable {
        reason: ProviderUnavailableReason,
    },
    /// Tried and failed. The message is for a person.
    Failed {
        message: String,
    },
    InvalidWorkingDirectory {
        message: String,
    },
    UndoOutOfRange {
        requested: usize,
        available: usize,
    },
    RewindPointNotFound {
        turn: u64,
    },
    CodeRewindUnavailable {
        message: String,
    },
    /// The conversation moved on since the fact the command was based on.
    Stale {
        current: SeqNo,
    },
    /// Nothing by that id here: the addressed session is no longer live, or
    /// the one asked for does not exist.
    NotFound,
}

/// Why there is no model to talk to.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderUnavailableReason {
    NotConfigured,
    AuthenticationRequired,
    UnsupportedBuild,
}

/// The host, as a front end holds it. Filled by the host; the service key that
/// carries it is declared by whoever consumes it (`docs/adr/0021` §6).
#[async_trait]
pub trait HostControl: Send + Sync {
    async fn call(&self, command: HostCommand) -> Result<HostReply, HostError>;
    /// Every event from now on.
    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent>;
}

/// Everything a front end gets from a host: the handle protocol to what it runs
/// and host control over it.
///
/// The pair outlives any one agent. When the host replaces the live session it
/// rewires underneath: the front end keeps these channels, hears
/// [`HostEvent::SessionChanged`], and subscribes to the new session
/// (`docs/adr/0022` §2, §3).
pub struct HostConnection {
    /// The session live when the connection was made.
    pub session: String,
    pub commands: mpsc::UnboundedSender<crate::event::AgentCommand>,
    pub events: mpsc::UnboundedReceiver<crate::event::AgentEvent>,
    pub control: std::sync::Arc<dyn HostControl>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each command, reply, event and error exactly once. The matches are
    /// exhaustive on purpose: a new variant does not compile until it is
    /// sampled here.
    fn commands() -> Vec<HostCommand> {
        let all = vec![
            HostCommand::NewSession {
                session: "a".into(),
            },
            HostCommand::Resume {
                session: "a".into(),
                target: "b".into(),
            },
            HostCommand::SetReasoningEffort {
                session: "a".into(),
                level: Some(ReasoningEffort::XHigh),
            },
            HostCommand::ListSessions {
                working_dir: Some("/w".into()),
            },
        ];
        for c in &all {
            match c {
                HostCommand::NewSession { .. }
                | HostCommand::Resume { .. }
                | HostCommand::SetReasoningEffort { .. }
                | HostCommand::ListSessions { .. } => {}
            }
        }
        all
    }

    fn replies() -> Vec<HostReply> {
        let all = vec![
            HostReply::Done,
            HostReply::SessionChanged {
                session: "b".into(),
            },
            HostReply::Sessions {
                sessions: vec![StoredSession {
                    id: "b".into(),
                    title: Some("fix the parser".into()),
                    working_dir: Some("/w".into()),
                    created_at: 1,
                    updated_at: 2,
                    turns: 3,
                    needs_newer_version: true,
                }],
            },
        ];
        for r in &all {
            match r {
                HostReply::Done | HostReply::SessionChanged { .. } | HostReply::Sessions { .. } => {
                }
            }
        }
        all
    }

    fn events() -> Vec<HostEvent> {
        let all = vec![HostEvent::SessionChanged {
            session: "b".into(),
            previous: Some("a".into()),
        }];
        for e in &all {
            match e {
                HostEvent::SessionChanged { .. } => {}
            }
        }
        all
    }

    fn errors() -> Vec<HostError> {
        let all = vec![
            HostError::Busy {
                reason: "a turn is running".into(),
            },
            HostError::Cancelled,
            HostError::SessionInUse { id: "b".into() },
            HostError::Unavailable,
            HostError::ProviderUnavailable {
                reason: ProviderUnavailableReason::AuthenticationRequired,
            },
            HostError::Failed {
                message: "no".into(),
            },
            HostError::InvalidWorkingDirectory {
                message: "gone".into(),
            },
            HostError::UndoOutOfRange {
                requested: 4,
                available: 2,
            },
            HostError::RewindPointNotFound { turn: 9 },
            HostError::CodeRewindUnavailable {
                message: "not a repository".into(),
            },
            HostError::Stale { current: 42 },
            HostError::NotFound,
        ];
        for e in &all {
            match e {
                HostError::Busy { .. }
                | HostError::Cancelled
                | HostError::SessionInUse { .. }
                | HostError::Unavailable
                | HostError::ProviderUnavailable { .. }
                | HostError::Failed { .. }
                | HostError::InvalidWorkingDirectory { .. }
                | HostError::UndoOutOfRange { .. }
                | HostError::RewindPointNotFound { .. }
                | HostError::CodeRewindUnavailable { .. }
                | HostError::Stale { .. }
                | HostError::NotFound => {}
            }
        }
        all
    }

    fn crosses<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value, "{json}");
    }

    /// The contract can go over a wire: every command, reply, event and error
    /// comes back as it left.
    #[test]
    fn every_host_variant_crosses_the_wire_unchanged() {
        commands().iter().for_each(crosses);
        replies().iter().for_each(crosses);
        events().iter().for_each(crosses);
        errors().iter().for_each(crosses);
        let result: Result<HostReply, HostError> = Err(HostError::NotFound);
        crosses(&result);
    }

    /// A command about the live session says which one; a question about the
    /// store does not have to.
    #[test]
    fn a_command_on_the_live_session_names_it() {
        for command in commands() {
            let expected = match &command {
                HostCommand::ListSessions { .. } => None,
                _ => Some("a"),
            };
            assert_eq!(command.addressed(), expected, "{command:?}");
        }
    }
}
