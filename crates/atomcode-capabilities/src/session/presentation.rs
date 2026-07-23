//! UI-only session replay data. These DTOs deliberately do not use kernel `Message`,
//! so presentation content cannot accidentally enter provider context.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{SessionResult, SessionStoreError};

pub const PRESENTATION_VERSION: u32 = 1;
pub const MAX_PRESENTATION_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PRESENTATION_ENTRIES: usize = 100_000;
pub const MAX_PRESENTATION_TEXT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisplayAnchor {
    AtStart,
    AfterTurn { turn_id: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationEntry {
    pub anchor: DisplayAnchor,
    pub role: PresentationRole,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationFile {
    pub v: u32,
    pub entries: Vec<PresentationEntry>,
}

impl Default for PresentationFile {
    fn default() -> Self {
        Self {
            v: PRESENTATION_VERSION,
            entries: Vec::new(),
        }
    }
}

impl PresentationFile {
    pub(crate) fn validate(&self) -> SessionResult<()> {
        if self.v > PRESENTATION_VERSION {
            return Err(SessionStoreError::FutureSchema {
                kind: "presentation",
                found: self.v,
                supported: PRESENTATION_VERSION,
            });
        }
        if self.v != PRESENTATION_VERSION {
            return Err(SessionStoreError::Corrupt {
                kind: "presentation",
                message: format!("unsupported historical schema v{}", self.v),
            });
        }
        if self.entries.len() > MAX_PRESENTATION_ENTRIES {
            return Err(SessionStoreError::TooLarge {
                kind: "presentation entries",
                limit: MAX_PRESENTATION_ENTRIES,
                actual: self.entries.len(),
            });
        }
        for entry in &self.entries {
            if matches!(entry.anchor, DisplayAnchor::AfterTurn { turn_id: 0 }) {
                return Err(corrupt_anchor("AfterTurn requires a non-zero turn id"));
            }
            if entry.text.len() > MAX_PRESENTATION_TEXT_BYTES {
                return Err(SessionStoreError::TooLarge {
                    kind: "presentation text",
                    limit: MAX_PRESENTATION_TEXT_BYTES,
                    actual: entry.text.len(),
                });
            }
        }
        Ok(())
    }

    pub fn retain_turns(&mut self, surviving_turn_ids: &BTreeSet<u64>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| match entry.anchor {
            DisplayAnchor::AtStart => true,
            DisplayAnchor::AfterTurn { turn_id } => surviving_turn_ids.contains(&turn_id),
        });
        before - self.entries.len()
    }
}

/// One completed legacy turn boundary produced by the compatibility parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyTurnBoundary {
    pub after_message: usize,
    pub turn_id: u64,
}

/// Convert a legacy mutable message offset exactly once at import time.
pub fn anchor_from_legacy_position(
    after_message: usize,
    boundaries: &[LegacyTurnBoundary],
) -> SessionResult<DisplayAnchor> {
    let mut previous_after = 0usize;
    let mut previous_turn = 0u64;
    for boundary in boundaries {
        if boundary.after_message == 0 || boundary.after_message <= previous_after {
            return Err(corrupt_anchor(
                "legacy turn boundaries must have strictly increasing positive message offsets",
            ));
        }
        if boundary.turn_id == 0 || boundary.turn_id <= previous_turn {
            return Err(corrupt_anchor(
                "legacy turn boundaries must have strictly increasing non-zero turn ids",
            ));
        }
        previous_after = boundary.after_message;
        previous_turn = boundary.turn_id;
    }
    if after_message == 0 {
        return Ok(DisplayAnchor::AtStart);
    }
    boundaries
        .iter()
        .find(|boundary| after_message <= boundary.after_message)
        .map(|boundary| DisplayAnchor::AfterTurn {
            turn_id: boundary.turn_id,
        })
        .ok_or_else(|| {
            corrupt_anchor(format!(
                "legacy after_message {after_message} is outside the completed turn map"
            ))
        })
}

pub(crate) fn corrupt_anchor(message: impl Into<String>) -> SessionStoreError {
    SessionStoreError::Corrupt {
        kind: "presentation anchor",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundary(after_message: usize, turn_id: u64) -> LegacyTurnBoundary {
        LegacyTurnBoundary {
            after_message,
            turn_id,
        }
    }

    #[test]
    fn legacy_position_maps_to_start_or_owning_completed_turn() {
        let turns = [boundary(3, 11), boundary(6, 12)];

        assert_eq!(
            anchor_from_legacy_position(0, &turns).unwrap(),
            DisplayAnchor::AtStart
        );
        assert_eq!(
            anchor_from_legacy_position(1, &turns).unwrap(),
            DisplayAnchor::AfterTurn { turn_id: 11 }
        );
        assert_eq!(
            anchor_from_legacy_position(3, &turns).unwrap(),
            DisplayAnchor::AfterTurn { turn_id: 11 }
        );
        assert_eq!(
            anchor_from_legacy_position(4, &turns).unwrap(),
            DisplayAnchor::AfterTurn { turn_id: 12 }
        );
        assert_eq!(
            anchor_from_legacy_position(6, &turns).unwrap(),
            DisplayAnchor::AfterTurn { turn_id: 12 }
        );
    }

    #[test]
    fn legacy_position_rejects_missing_or_ambiguous_turn_map() {
        assert!(matches!(
            anchor_from_legacy_position(1, &[]),
            Err(SessionStoreError::Corrupt {
                kind: "presentation anchor",
                ..
            })
        ));
        assert!(anchor_from_legacy_position(2, &[boundary(3, 0)]).is_err());
        assert!(anchor_from_legacy_position(2, &[boundary(3, 1), boundary(3, 2)]).is_err());
        assert!(anchor_from_legacy_position(7, &[boundary(3, 1), boundary(6, 2)]).is_err());
    }

    #[test]
    fn pruning_keeps_start_and_only_surviving_turn_anchors() {
        let mut file = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![
                PresentationEntry {
                    anchor: DisplayAnchor::AtStart,
                    role: PresentationRole::Assistant,
                    text: "start".into(),
                },
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 1 },
                    role: PresentationRole::User,
                    text: "gone".into(),
                },
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 2 },
                    role: PresentationRole::Assistant,
                    text: "keep".into(),
                },
            ],
        };

        assert_eq!(file.retain_turns(&BTreeSet::from([2])), 1);
        assert_eq!(file.entries.len(), 2);
        assert_eq!(file.entries[0].anchor, DisplayAnchor::AtStart);
        assert_eq!(
            file.entries[1].anchor,
            DisplayAnchor::AfterTurn { turn_id: 2 }
        );
    }

    #[test]
    fn validation_rejects_zero_native_turn_anchor() {
        let file = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AfterTurn { turn_id: 0 },
                role: PresentationRole::Assistant,
                text: "invalid".into(),
            }],
        };

        assert!(matches!(
            file.validate(),
            Err(SessionStoreError::Corrupt {
                kind: "presentation anchor",
                ..
            })
        ));
    }

    #[test]
    fn wire_schema_is_versioned_and_has_no_kernel_message_shape() {
        let file = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AfterTurn { turn_id: 9 },
                role: PresentationRole::Assistant,
                text: "display only".into(),
            }],
        };
        let json = serde_json::to_value(&file).unwrap();

        assert_eq!(json["v"], 1);
        assert_eq!(json["entries"][0]["anchor"]["kind"], "after_turn");
        assert_eq!(json["entries"][0]["anchor"]["turn_id"], 9);
        assert!(json["entries"][0].get("tool_calls").is_none());
        assert_eq!(
            serde_json::from_value::<PresentationFile>(json).unwrap(),
            file
        );
    }
}
