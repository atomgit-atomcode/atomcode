//! The per-turn record `recall` and `/worklog` read.
//!
//! A session's log is its record now (`docs/adr/0024` §14):
//! [`turn_records`](super::events::turn_records) folds these out of the facts. The
//! shape stays because two things still carry it — the `<id>.jsonl` transcript of a
//! session a released build kept and nobody has resumed since, and the readers,
//! which should not care which of the two a turn came from. Nothing appends
//! transcripts any more.

use serde::{Deserialize, Serialize};

pub const RECORD_VERSION: u32 = 1;

/// One completed turn, RAW (no redaction) — one JSON object per `<id>.jsonl` line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    /// `.jsonl` RECORD SCHEMA VERSION — same forward-compat seam as
    /// `SessionMeta.v`: new records write 1, pre-version lines read as 0
    /// (`serde(default)`); additive fields keep the `v`, breaking changes bump it.
    #[serde(default)]
    pub v: u32,
    /// Epoch milliseconds when the user prompt was accepted. Added after v1;
    /// older records leave it absent instead of fabricating a send time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// epoch MILLISECONDS, UTC — stamped by L1 at flush.
    pub ts: i64,
    /// Human-readable RFC-3339 mirror of `ts` (display / debug).
    pub iso: String,
    pub session_id: String,
    /// Kernel `TurnCtx.turn_id` (monotonic within the session).
    pub turn_id: u64,
    /// RESERVED for `/undo`: a turn rewound past by a later `/undo` stays in the
    /// transcript (decision: kept, not deleted) flagged `undone`. v1 always writes
    /// `false` — the marking mechanism is DEFERRED with the rest of `/undo` wiring
    /// (the append-only jsonl needs a side index or a rewrite pass to set it after the
    /// fact); the field + the recall display are forward-compatible for when it lands.
    #[serde(default)]
    pub undone: bool,
    /// The original user prompt text (raw). A mid-turn synthetic continuation
    /// (`offer_continuation`) is NOT recorded as user input.
    pub user: String,
    /// Final assistant text across ALL rounds of the turn (raw).
    pub assistant: String,
    /// Concatenated reasoning across rounds, if any.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default)]
    pub tools: Vec<ToolRecord>,
    pub usage: UsageRecord,
}

/// Display clocks retained for one completed turn. `started_at` is optional for
/// transcripts written before per-turn start timestamps were introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnTimestamp {
    pub started_at: Option<i64>,
    pub completed_at: i64,
}

/// One tool call + its paired result within a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRecord {
    pub name: String,
    pub args: String,
    pub result: String,
    #[serde(default)]
    pub is_error: bool,
}

/// Turn-aggregated token usage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub prompt: u32,
    pub completion: u32,
    #[serde(default)]
    pub cached: u32,
}
