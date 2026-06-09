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

pub mod current_date;
pub mod manager;
pub mod recall;
pub mod snapshot;
pub mod transcript;
pub use current_date::CurrentDateHook;
pub use manager::{SessionManager, SessionMeta, TurnStat};
pub use recall::{KeywordIndex, RecallIndex, RecallTool};
pub use snapshot::SnapshotHook;
pub use transcript::{ToolRecord, TranscriptHook, TurnRecord, UsageRecord};

/// Current wall-clock as epoch MILLISECONDS, UTC. The single L1 time source the
/// persistence hooks stamp records with (the kernel stays clock-free).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The atomcode config/data root: `$ATOMCODE_HOME` if set & non-empty, else
/// `~/.atomcode`. Mirrors `atomcode_core::config::Config::config_dir` so the new
/// stack's `sessions/` lands in the SAME tree as production's. NOTE: the core helper's
/// extra `$SUDO_USER`/getpwnam home resolution is intentionally NOT ported (same
/// deliberate L1 simplification as [`crate::mcp`]'s `util::config_dir`) — under `sudo`
/// WITHOUT `$ATOMCODE_HOME` set the two roots can diverge; setting `$ATOMCODE_HOME`
/// (checked first, byte-identical to production) keeps them aligned.
pub(crate) fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("ATOMCODE_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".atomcode")
}
