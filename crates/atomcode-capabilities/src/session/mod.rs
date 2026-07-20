//! Session persistence + cross-session recall (L1).
//!
//! Two on-disk tiers, both under `$ATOMCODE_HOME/sessions/<project_hash>/` (the SAME
//! bucket scheme production uses, so old `<id>.json` and new sessions coexist):
//! - `<id>.snapshot` — the kernel [`SessionSnapshot`](atomcode_kernel::message::SessionSnapshot)
//!   (the COMPACTED working set), rewritten every turn → used to RESUME. Lossy over
//!   time (bounded by the context window); NOT the system of record.
//! - `<id>.jsonl` — an append-only, NEVER-compacted, one-record-per-turn RAW transcript
//!   → the ground truth for RECALL (the agent retrieving any past exchange, including
//!   from OTHER sessions of the same project). Compaction shrinks the snapshot; it never
//!   touches the transcript.
//! - `<id>.meta` — fast-listing metadata (name / dirs / timestamps / turn_stats). JSON
//!   content with a `.meta` extension that deliberately AVOIDS production's `*.json`
//!   session glob, so the two schemes share a project dir without the production lister
//!   choking on our files.
//!
//! Everything is driven by EXISTING kernel seams (zero core, zero kernel change): the
//! [`SnapshotHook`] / [`TranscriptHook`] hang off the `turn_complete` terminal hook so
//! they persist HOWEVER a turn ended; `recall` is a normal tool; current-date injection
//! is an append-only tail in `pre_request`. WALL-CLOCK LIVES ONLY HERE — the kernel is
//! deliberately clock-free — so L1 stamps every record via [`now_ms`].

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod context;
pub mod instructions;
pub mod manager;
pub mod presentation;
pub mod recall;
pub mod snapshot;
pub mod status_reminder;
pub mod transcript;
pub use context::SessionContextHook;
pub use manager::{
    CatalogDiagnostic, CatalogDiagnosticKind, CatalogEntry, CatalogLocation, CatalogPresence,
    CatalogScan, ImportInfo, ImportKind, LoadedSession, SessionLease, SessionManager, SessionMeta,
    SessionResult, SessionStoreError, StorageOwner, TurnStat,
};
pub use presentation::{
    anchor_from_legacy_position, DisplayAnchor, LegacyTurnBoundary, PresentationEntry,
    PresentationFile, PresentationRole,
};
pub use recall::{KeywordIndex, RecallIndex, RecallTool};
pub use snapshot::SnapshotHook;
pub use status_reminder::StatusReminderHook;
pub use transcript::{ToolRecord, TranscriptHook, TurnRecord, UsageRecord};

/// Current wall-clock as epoch MILLISECONDS, UTC. The single L1 time source the
/// persistence hooks stamp records with (the kernel stays clock-free).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The atomcode config/data root — delegates to the crate-shared
/// [`crate::paths::config_dir`] (one home for the rule + its documented `sudo`
/// divergence from production).
pub(crate) fn config_dir() -> PathBuf {
    crate::paths::config_dir()
}
