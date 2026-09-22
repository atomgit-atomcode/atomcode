//! Session persistence + cross-session recall (L1).
//!
//! Sessions live under `$ATOMCODE_HOME/sessions/<project_hash>/`. A session's one
//! authority is its event log (`docs/adr/0024`, [`events`]):
//! - `<id>.events` — the header, then every committed fact, appended as it
//!   happens and never rewritten. A resume replays it; the conversation a
//!   snapshot reader wants is projected from it; `recall` and `/worklog` fold
//!   their per-turn records out of it.
//! - `<id>.index` — fast-listing metadata (name / dirs / timestamps / turn_stats),
//!   beside `.ui`, `.rewind`, `.rewind.txn` and `.todos` sidecars.
//!
//! A session a released build kept as `<id>.snapshot` + `<id>.meta` + an
//! `<id>.jsonl` transcript is read as it is and converted the first time it is
//! opened; its files are moved aside, not deleted.
//!
//! What is not the log is driven by kernel seams: [`SnapshotHook`] keeps the
//! index's per-turn statistics and the rewind ledger at `turn_complete`; `recall`
//! and `list_sessions` are normal tools. WALL-CLOCK LIVES ONLY HERE — the kernel is
//! deliberately clock-free — so L1 stamps what it writes via [`now_ms`].

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod context;
// Moved to the crate root so a consumer can take the loader without the whole
// session subsystem. Re-exported here so `session::instructions::…` still resolves.
pub use crate::instructions;
pub mod events;
pub mod manager;
pub mod presentation;
pub mod recall;
pub mod rewind;
pub mod session_list;
pub mod snapshot;
pub mod status_reminder;
pub mod transcript;
mod usage_provider;
pub mod worklog;
pub use context::SessionContextHook;
pub use manager::{
    aggregate_session_cost, CatalogDiagnostic, CatalogDiagnosticKind, CatalogEntry,
    CatalogLocation, CatalogPresence, CatalogScan, DetachedUsageRecorder, ForkInfo, ImportInfo,
    ImportKind, LoadedSession, ModelCostSummary, ModelUsageStat, NativeSessionRepairOutcome,
    SessionCostReport, SessionLease, SessionManager, SessionMeta, SessionResult, SessionStoreError,
    StorageOwner, TokenBreakdown, TurnStat,
};
pub use presentation::{
    anchor_from_legacy_position, DisplayAnchor, LegacyTurnBoundary, PresentationEntry,
    PresentationFile, PresentationRole,
};
pub use recall::{KeywordIndex, RecallIndex, RecallRequest, RecallTool};
pub use rewind::{
    FileChangeSummary, RewindPoint, WorkspaceCheckpoint, WorkspaceCheckpointError,
    WorkspaceRestoreReceipt,
};
pub use session_list::ListSessionsTool;
pub use snapshot::{CodeRewindUnavailable, RewindTransactionReceipt, SnapshotHook};
pub use status_reminder::StatusReminderHook;
pub use transcript::{ToolRecord, TurnRecord, TurnTimestamp, UsageRecord};
pub use usage_provider::UsageRecordingProvider;
pub use worklog::{
    build_worklog_prompt, collect_day_turns, local_day_window_ms, resolve_worklog_date, WorklogTurn,
};

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
