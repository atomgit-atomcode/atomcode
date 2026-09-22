//! Failure modes of the plugin runtime itself.
//!
//! These are *composition* errors — a config row naming a plugin nobody
//! registered, two plugins racing for one service slot, a dependency nobody
//! provides. Errors a plugin raises from its own `apply` are wrapped in
//! [`PlexusError::Apply`] so the failing entry id is never lost.

use std::fmt;

#[derive(Debug)]
#[non_exhaustive]
pub enum PlexusError {
    /// A config row named a plugin the registry does not know. Carries both the
    /// row id and the plugin name because a patch usually gets one of the two right.
    UnknownPlugin { entry: String, plugin: String },
    /// Two fibers tried to fill the same service slot in the same realm. The
    /// runtime refuses silent last-write-wins: a replacement must unload the
    /// incumbent (that is what patching a row does) or claim its own realm.
    ServiceConflict {
        name: &'static str,
        held_by: String,
        claimed_by: String,
    },
    /// A consumer read a slot that is empty. Distinct from [`Self::Deadlock`]:
    /// this is a *runtime* read, not a mount-time wait.
    ServiceMissing { name: &'static str },
    /// Mounting reached a fixed point with rows still waiting. Each entry is
    /// `(row id, services it is still missing)` — the shape you need to tell
    /// "forgot to mount the provider" from "typo in `inject`".
    Deadlock { pending: Vec<(String, Vec<String>)> },
    /// The plugin's own `apply` returned an error.
    Apply { entry: String, message: String },
    /// The config tree or a patch layer could not be read.
    Config(String),
}

impl fmt::Display for PlexusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPlugin { entry, plugin } => write!(
                f,
                "entry `{entry}` names plugin `{plugin}`, which is not in the registry"
            ),
            Self::ServiceConflict {
                name,
                held_by,
                claimed_by,
            } => write!(
                f,
                "service `{name}` is already provided by `{held_by}`; `{claimed_by}` cannot claim it \
                 (unload the incumbent or isolate a realm)"
            ),
            Self::ServiceMissing { name } => write!(f, "service `{name}` is not provided"),
            Self::Deadlock { pending } => {
                write!(f, "mounting stalled with unsatisfied dependencies:")?;
                for (entry, missing) in pending {
                    write!(f, "\n  - `{entry}` waits for: {}", missing.join(", "))?;
                }
                Ok(())
            }
            Self::Apply { entry, message } => write!(f, "entry `{entry}` failed to apply: {message}"),
            Self::Config(m) => write!(f, "config error: {m}"),
        }
    }
}

impl std::error::Error for PlexusError {}

pub type Result<T> = std::result::Result<T, PlexusError>;
