//! Protocol-neutral conversation replay projection.
//!
//! Both ACP chains need to surface a persisted session's history back to the
//! client — v1 via `session/load`, v2 via `session/resume` with
//! `replayFrom: { "type": "start" }`. The persisted read and the display rules
//! (which messages are visible conversation vs. kernel-internal bookkeeping) are
//! identical across both; only the wire shape differs. This module owns the
//! shared half: it reads the native aggregate and projects it onto a neutral
//! [`ReplayEntry`] sequence in conversation order. Each chain then maps those
//! entries onto its own `session/update` wire shape (see
//! [`crate::acp::dispatch::replay_entries_to_v1_updates`] and
//! [`crate::acp::v2::replay_entries_to_v2_updates`]).
//!
//! Display rules mirror the daemon's display merge: synthetic / cold-summary /
//! system-reminder entries are hidden; kernel tool results (User messages
//! carrying a `tool_call_id`) are not user input and are skipped as
//! conversation, but their content feeds the paired [`ReplayEntry::ToolCall`];
//! presentation entries anchored at a pruned/missing turn are dropped
//! best-effort instead of failing the whole replay.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use atomcode_capabilities::reminder::is_system_reminder;
use atomcode_capabilities::session::{
    DisplayAnchor, PresentationEntry, PresentationRole, SessionManager,
};
use atomcode_kernel::message::{ImageContent, Role as KernelRole, LEGACY_COLD_SUMMARY_ORIGIN};

/// One displayable conversation unit, in conversation order. The `kind` drives
/// the wire mapping; `text` carries the message/thought text and `images` the
/// (user-only) attached images. [`ReplayEntry::ToolCall`] reconstructs one tool
/// call from the persisted assistant message, paired with its recorded result
/// (hidden as a standalone message) so a restored session shows the same
/// `tool_call` / `tool_call_update` records a live turn emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayEntry {
    User {
        text: String,
        images: Vec<ImageContent>,
    },
    Assistant {
        text: String,
    },
    Thought {
        text: String,
    },
    ToolCall {
        /// Kernel tool call id (paired with the result's `tool_call_id`).
        id: String,
        name: String,
        /// Raw JSON arguments string, as persisted.
        arguments: String,
        /// Recorded result, if one was persisted: `(content, is_error)`.
        result: Option<(String, bool)>,
    },
}

/// Load the persisted native aggregate and project it to a neutral, ordered
/// replay sequence.
///
/// Fails with a human-readable message when the session cannot be loaded
/// (unknown/corrupt/missing artifact). The caller maps that message onto its
/// own JSON-RPC error shape.
pub fn build_replay_entries(
    native_id: &str,
    working_dir: &Path,
) -> Result<Vec<ReplayEntry>, String> {
    let manager = SessionManager::for_project(working_dir);
    let loaded = manager
        .load_native_session(native_id)
        .map_err(|e| format!("acp: replay failed to load session history: {e}"))?;

    // Presentation entries keyed by the snapshot-message position they follow
    // (`AtStart` → 0, `AfterTurn{turn_id}` → that turn's `after_message`).
    let mut presentation_at: BTreeMap<usize, Vec<&PresentationEntry>> = BTreeMap::new();
    for entry in &loaded.presentation.entries {
        let position = match entry.anchor {
            DisplayAnchor::AtStart => 0,
            DisplayAnchor::AfterTurn { turn_id } => {
                let Some(stat) = loaded
                    .meta
                    .turn_stats
                    .iter()
                    .find(|s| s.position_valid && s.turn_id == turn_id)
                else {
                    // A pruned/legacy turn: drop the display-only entry rather
                    // than failing the whole replay.
                    eprintln!(
                        "acp: replay: skipping presentation entry for missing turn {turn_id}"
                    );
                    continue;
                };
                stat.after_message
            }
        };
        presentation_at.entry(position).or_default().push(entry);
    }

    // Tool results are hidden as standalone conversation entries, but they
    // carry the recorded outcome of each call. Pair them up front so the
    // assistant `ToolCall` entries below resolve by `tool_call_id` regardless
    // of when the result message appears in the snapshot.
    let mut tool_results: HashMap<&str, (&str, bool)> = HashMap::new();
    for message in &loaded.snapshot.messages {
        if let Some(id) = &message.tool_call_id {
            tool_results.insert(id.as_str(), (message.text.as_str(), message.is_error));
        }
    }

    let mut entries: Vec<ReplayEntry> = Vec::new();
    push_presentation_entries(&mut presentation_at, 0, &mut entries);
    for (index, message) in loaded.snapshot.messages.iter().enumerate() {
        // Hide kernel-internal entries the way the daemon display merge does:
        // synthetic summaries, cold-summary placeholders, system reminders, and
        // tool-result echoes are never shown as conversation.
        let hidden = message.synthetic
            || message.internal_origin.as_deref() == Some(LEGACY_COLD_SUMMARY_ORIGIN)
            || (message.role == KernelRole::User
                && (message.tool_call_id.is_some() || is_system_reminder(&message.text)));
        if !hidden {
            match message.role {
                KernelRole::User => entries.push(ReplayEntry::User {
                    text: message.text.clone(),
                    images: message.images.clone(),
                }),
                KernelRole::Assistant => {
                    if let Some(reasoning) = message
                        .reasoning
                        .as_deref()
                        .filter(|r| !r.trim().is_empty())
                    {
                        entries.push(ReplayEntry::Thought {
                            text: reasoning.to_string(),
                        });
                    }
                    if !message.text.trim().is_empty() {
                        entries.push(ReplayEntry::Assistant {
                            text: message.text.clone(),
                        });
                    }
                    // Reconstruct tool calls from the persisted assistant
                    // message, pairing each call with its recorded result so a
                    // restored session shows the same tool-call records a live
                    // turn emitted.
                    for call in &message.tool_calls {
                        entries.push(ReplayEntry::ToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            result: tool_results
                                .get(call.id.as_str())
                                .map(|(text, is_error)| (text.to_string(), *is_error)),
                        });
                    }
                }
                KernelRole::System | KernelRole::Tool => {}
            }
        }
        push_presentation_entries(&mut presentation_at, index + 1, &mut entries);
    }
    // Entries anchored past the last snapshot message (best effort).
    let leftover: Vec<usize> = presentation_at.keys().copied().collect();
    for position in leftover {
        push_presentation_entries(&mut presentation_at, position, &mut entries);
    }
    Ok(entries)
}

/// Append the presentation entries anchored at `position` to the replay
/// sequence. System-reminder rows are hidden; missing positions are a no-op.
fn push_presentation_entries(
    presentation_at: &mut BTreeMap<usize, Vec<&PresentationEntry>>,
    position: usize,
    entries: &mut Vec<ReplayEntry>,
) {
    for entry in presentation_at.remove(&position).unwrap_or_default() {
        if entry.role == PresentationRole::User && is_system_reminder(&entry.text) {
            continue;
        }
        match entry.role {
            PresentationRole::User => entries.push(ReplayEntry::User {
                text: entry.text.clone(),
                images: Vec::new(),
            }),
            PresentationRole::Assistant => entries.push(ReplayEntry::Assistant {
                text: entry.text.clone(),
            }),
        }
    }
}
