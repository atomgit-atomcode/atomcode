//! Durable checkpoint seam for out-of-turn conversation mutations.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::message::SessionSnapshot;

/// A typed failure returned when a prepared compaction snapshot cannot be saved.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompactionCheckpointError {
    message: String,
}

impl CompactionCheckpointError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CompactionCheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for CompactionCheckpointError {}

/// The single durable writer consulted before a manual compaction becomes live.
///
/// Implementations must return only after `snapshot` is available to the normal
/// resume path. The kernel calls this synchronously while it exclusively owns the
/// conversation, then commits the already-prepared candidate without another await.
pub trait CompactionCheckpoint: Send + Sync {
    fn save(&self, snapshot: &SessionSnapshot) -> Result<(), CompactionCheckpointError>;
}
