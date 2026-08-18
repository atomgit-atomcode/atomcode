//! `SessionManager` — the two-tier on-disk session store + its fast-listing metadata.
//!
//! Pure storage: no kernel coupling beyond serializing the kernel's `SessionSnapshot`.
//! The hooks (snapshot / transcript) and the recall tool call into this; the manager
//! itself does only file IO, so it is fully unit-testable with a temp root.

#[cfg(test)]
use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Barrier;
use std::sync::{Arc, Mutex, OnceLock};

use atomcode_kernel::message::{Message, Role, SessionSnapshot, SNAPSHOT_VERSION};
use serde::{de::IgnoredAny, Deserialize, Serialize};

use super::presentation::{
    DisplayAnchor, PresentationEntry, PresentationFile, PresentationRole, MAX_PRESENTATION_BYTES,
};
use super::transcript::{TurnTimestamp, RECORD_VERSION};

/// Fast-listing metadata for ONE session — read to populate a `/resume` picker WITHOUT
/// parsing the (large) snapshot / transcript files. Persisted as `<id>.meta`.
pub const META_VERSION: u32 = 1;

// These are persistence safety ceilings, not product quotas. They are deliberately
// above normal context-window payloads, but finite so a corrupted/untrusted session file
// cannot force an unbounded allocation before serde gets a chance to reject it.
pub const MAX_SESSION_ID_BYTES: usize = 128;
pub const MAX_META_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_REWIND_TRANSACTION_BYTES: usize = MAX_SNAPSHOT_BYTES + MAX_META_BYTES;
const INFLIGHT_SNAPSHOT_VERSION: u32 = 1;
pub const MAX_LEGACY_SESSION_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_JSONL_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_JSONL_BYTES: usize = 512 * 1024 * 1024;
pub const MAX_JSONL_LINES: usize = 1_000_000;
pub const MAX_SNAPSHOT_MESSAGES: usize = 100_000;
pub const MAX_META_TURN_STATS: usize = 100_000;
pub const MAX_STORED_STRING_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_IMAGE_BASE64_BYTES: usize = 32 * 1024 * 1024;

pub type SessionResult<T> = Result<T, SessionStoreError>;

#[derive(Debug)]
pub enum SessionStoreError {
    InvalidId {
        id: String,
        reason: &'static str,
    },
    NotFound {
        path: PathBuf,
    },
    SessionInUse {
        id: String,
        path: PathBuf,
    },
    AmbiguousId {
        query: String,
        matches: Vec<CatalogLocation>,
    },
    LeaseMismatch {
        id: String,
        expected: PathBuf,
        actual: PathBuf,
    },
    OwnershipConflict {
        id: String,
        owner: StorageOwner,
        operation: &'static str,
    },
    TooLarge {
        kind: &'static str,
        limit: usize,
        actual: usize,
    },
    FutureSchema {
        kind: &'static str,
        found: u32,
        supported: u32,
    },
    Corrupt {
        kind: &'static str,
        message: String,
    },
    UnsafeFile {
        path: PathBuf,
        reason: &'static str,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    UncertainCommit {
        id: String,
        commit_error: String,
        rollback_errors: Vec<String>,
    },
}

impl SessionStoreError {
    pub fn is_uncertain_commit(&self) -> bool {
        matches!(self, Self::UncertainCommit { .. })
    }

    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::InvalidId { .. } => io::ErrorKind::InvalidInput,
            Self::NotFound { .. } => io::ErrorKind::NotFound,
            Self::SessionInUse { .. } => io::ErrorKind::WouldBlock,
            Self::AmbiguousId { .. } | Self::LeaseMismatch { .. } => io::ErrorKind::InvalidInput,
            Self::OwnershipConflict { .. } => io::ErrorKind::PermissionDenied,
            Self::TooLarge { .. }
            | Self::FutureSchema { .. }
            | Self::Corrupt { .. }
            | Self::UnsafeFile { .. } => io::ErrorKind::InvalidData,
            Self::Io { source, .. } => source.kind(),
            Self::UncertainCommit { .. } => io::ErrorKind::Other,
        }
    }
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId { id, reason } => write!(f, "invalid session id {id:?}: {reason}"),
            Self::NotFound { path } => write!(f, "session file not found: {}", path.display()),
            Self::SessionInUse { id, .. } => {
                write!(f, "session {id:?} is already in use by another runtime")
            }
            Self::AmbiguousId { query, matches } => write!(
                f,
                "session query {query:?} is ambiguous across {} locations",
                matches.len()
            ),
            Self::LeaseMismatch {
                id,
                expected,
                actual,
            } => write!(
                f,
                "session lease for {id:?} belongs to {}, expected {}",
                actual.display(),
                expected.display()
            ),
            Self::OwnershipConflict {
                id,
                owner,
                operation,
            } => write!(
                f,
                "session {id:?} is owned by {owner:?}; {operation} is not allowed"
            ),
            Self::TooLarge {
                kind,
                limit,
                actual,
            } => {
                write!(f, "{kind} exceeds {limit} bytes/items (actual {actual})")
            }
            Self::FutureSchema {
                kind,
                found,
                supported,
            } => {
                write!(f, "{kind} schema v{found} > supported v{supported}")
            }
            Self::Corrupt { kind, message } => write!(f, "corrupt {kind}: {message}"),
            Self::UnsafeFile { path, reason } => {
                write!(f, "unsafe session file {}: {reason}", path.display())
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::UncertainCommit {
                id,
                commit_error,
                rollback_errors,
            } => write!(
                f,
                "session {id:?} commit failed ({commit_error}) and rollback was incomplete: {}",
                rollback_errors.join("; ")
            ),
        }
    }
}

impl std::error::Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<SessionStoreError> for io::Error {
    fn from(error: SessionStoreError) -> Self {
        io::Error::new(error.kind(), error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogLocation {
    pub id: String,
    pub project_bucket: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogPresence {
    LegacyOnly,
    NativeOnly,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    /// Root identity for an automatic busy-continue fork.
    pub fork_root_id: Option<String>,
    pub project_bucket: String,
    pub working_dir: PathBuf,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub message_count: usize,
    pub turn_count: usize,
    pub presence: CatalogPresence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogDiagnosticKind {
    Io,
    InvalidId,
    UnsafeFile,
    TooLarge,
    FutureSchema,
    Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogDiagnostic {
    pub project_bucket: Option<String>,
    pub path: PathBuf,
    pub kind: CatalogDiagnosticKind,
    pub message: String,
}

#[derive(Debug, Default, Clone)]
pub struct CatalogScan {
    pub entries: Vec<CatalogEntry>,
    pub diagnostics: Vec<CatalogDiagnostic>,
}

impl CatalogScan {
    pub fn latest(&self) -> Option<&CatalogEntry> {
        self.entries.first()
    }

    pub fn search_name(&self, query: &str) -> Vec<&CatalogEntry> {
        let query = query.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| entry.name.to_lowercase().contains(&query))
            .collect()
    }

    /// Resolve an exact id first, then a safe prefix. Directory order and timestamps
    /// never break ties: every multi-location match is explicitly ambiguous.
    pub fn find(&self, query: &str) -> SessionResult<Option<CatalogEntry>> {
        validate_session_query(query)?;
        let exact: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| entry.id == query)
            .collect();
        if !exact.is_empty() {
            return unique_catalog_match(query, exact);
        }
        unique_catalog_match(
            query,
            self.entries
                .iter()
                .filter(|entry| entry.id.starts_with(query))
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageOwner {
    Unconfirmed,
    Legacy,
    Native,
}

impl Default for StorageOwner {
    fn default() -> Self {
        Self::Unconfirmed
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    Full,
    MetadataOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportInfo {
    pub legacy_schema: String,
    pub source_sha256: String,
    pub importer_version: u32,
    pub kind: ImportKind,
}

/// Durable lineage for an automatic fork created because `--continue` found
/// its source session leased by another runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkInfo {
    pub root_id: String,
    pub parent_id: String,
    pub forked_at_ms: i64,
    pub base_message_count: u32,
    pub base_turn_count: u32,
}

/// How a session was created. Stored in [`SessionMeta`] so that automated
/// scheduled-task sessions can be excluded from user-facing history pickers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionOrigin {
    /// Started by the user (TUI, webui, CLI). The default for all legacy
    /// sessions that predate this field (`#[serde(default)]`).
    #[default]
    Manual,
    /// Started by the scheduled-tasks runner. Hidden from normal `/resume`
    /// and sidebar pickers; visible in a dedicated scheduled-tasks view.
    Scheduled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// `.meta` SCHEMA VERSION — the forward-compat seam (`.snapshot` has
    /// `SNAPSHOT_VERSION`; this file format was missing one). New files write 1; a
    /// pre-version file reads as 0 (`serde(default)`). Evolution rule: additive
    /// fields stay at the same `v` (with their own `serde(default)`); a breaking
    /// change bumps it and the reader branches.
    #[serde(default)]
    pub v: u32,
    pub id: String,
    /// Display title — auto by default, set by a user `/rename` (then `user_renamed`).
    pub name: String,
    #[serde(default)]
    pub user_renamed: bool,
    /// True once the AI session namer has assigned `name`. Kept separate from
    /// `user_renamed` so a resumed session does not run the one-shot namer again.
    #[serde(default)]
    pub ai_named: bool,
    /// Durable storage authority. Missing on pre-S2 metadata, which must be
    /// resolved under the session lease instead of guessed by readers.
    #[serde(default)]
    pub owner: StorageOwner,
    /// Provenance of a committed legacy cutover. Fresh native sessions leave it
    /// empty; it is deliberately independent from `owner`.
    #[serde(default)]
    pub import_info: Option<ImportInfo>,
    /// Automatic busy-continue fork provenance. This is deliberately separate
    /// from `import_info`, which only describes legacy-format cutover.
    #[serde(default)]
    pub fork_info: Option<ForkInfo>,
    pub working_dir: String,
    /// epoch MILLISECONDS, UTC.
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub turn_count: u32,
    #[serde(default)]
    pub message_count: u32,
    /// Per-turn stats so a resume can re-render the `✓ … tokens` dividers without
    /// replaying the model. (Kernel A4 could fold these into `SessionSnapshot`; until
    /// then they live here, where wall-clock `duration_ms` belongs anyway.)
    #[serde(default)]
    pub turn_stats: Vec<TurnStat>,
    /// Provider usage without a live turn position: calls outside the primary
    /// loop (subagents, review, LLM compaction) plus usage archived when
    /// undo/compaction removes the corresponding turn divider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detached_model_usage: Vec<ModelUsageStat>,
    /// Legacy usage moved out of turn stats when undo/compaction removes the
    /// corresponding presentation turn. It remains billable but cannot be
    /// attributed to a provider/model written by older metadata.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub detached_unattributed_tokens: u64,
    /// How this session was created. Defaults to [`SessionOrigin::Manual`] so
    /// that sessions written before this field existed deserialize correctly.
    #[serde(default)]
    pub origin: SessionOrigin,
}

impl SessionMeta {
    /// Fresh metadata for a new session — auto-named `session-<id>`, both timestamps
    /// `now_ms`, empty stats.
    pub fn new(id: impl Into<String>, working_dir: impl Into<String>, now_ms: i64) -> Self {
        let id = id.into();
        Self {
            v: META_VERSION,
            name: format!("session-{id}"),
            id,
            user_renamed: false,
            ai_named: false,
            owner: StorageOwner::Unconfirmed,
            import_info: None,
            fork_info: None,
            working_dir: working_dir.into(),
            created_at: now_ms,
            updated_at: now_ms,
            turn_count: 0,
            message_count: 0,
            turn_stats: Vec::new(),
            detached_model_usage: Vec::new(),
            detached_unattributed_tokens: 0,
            origin: SessionOrigin::Manual,
        }
    }

    /// Remove turn stats selected by `predicate` while preserving their usage
    /// in the position-independent cost ledger.
    pub fn archive_turn_stats_where(
        &mut self,
        mut predicate: impl FnMut(&TurnStat) -> bool,
    ) -> Vec<TurnStat> {
        let mut retained = Vec::with_capacity(self.turn_stats.len());
        let mut archived = Vec::new();
        for turn in std::mem::take(&mut self.turn_stats) {
            if !predicate(&turn) {
                retained.push(turn);
                continue;
            }
            if turn.model_usage.is_empty() {
                self.detached_unattributed_tokens = self
                    .detached_unattributed_tokens
                    .saturating_add(u64::from(turn.total_tokens));
            } else {
                for usage in &turn.model_usage {
                    merge_model_usage(&mut self.detached_model_usage, usage.clone());
                }
            }
            archived.push(turn);
        }
        self.turn_stats = retained;
        archived
    }

    /// Reverse only the cost-ledger contribution made by
    /// [`archive_turn_stats_where`]. Concurrent detached usage remains intact.
    pub fn remove_archived_turn_usage(&mut self, archived: &[TurnStat]) {
        for turn in archived {
            if turn.model_usage.is_empty() {
                self.detached_unattributed_tokens = self
                    .detached_unattributed_tokens
                    .saturating_sub(u64::from(turn.total_tokens));
            } else {
                for usage in &turn.model_usage {
                    subtract_model_usage(&mut self.detached_model_usage, usage);
                }
            }
        }
    }

    /// Whether a stored title is an untouched/generated placeholder that may be
    /// replaced by a deterministic first-prompt fallback.
    pub fn name_needs_fallback(name: &str, session_id: &str) -> bool {
        let name = name.trim_start();
        let generated_session_name = name.strip_prefix("session-").is_some_and(|suffix| {
            suffix == session_id
                || (suffix.len() >= 10 && suffix.bytes().all(|byte| byte.is_ascii_digit()))
        });
        name == "default" || generated_session_name || strip_leading_image_markers(name) != name
    }

    /// Replace an untouched placeholder with the first real user prompt.
    /// This is the durable fallback when optional AI title generation is
    /// disabled or fails; it deliberately leaves `ai_named` false so a later
    /// generated title may still refine the name.
    pub fn auto_name_from_messages(&mut self, messages: &[Message]) {
        if self.user_renamed || self.ai_named || !Self::name_needs_fallback(&self.name, &self.id) {
            return;
        }
        let Some(text) = messages
            .iter()
            .filter(|message| {
                message.role == Role::User
                    && !message.synthetic
                    && !crate::reminder::is_system_reminder(&message.text)
            })
            .map(|message| strip_leading_image_markers(message.text.trim()))
            .find(|text| !text.is_empty())
        else {
            return;
        };
        let name: String = text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .take(40)
            .collect();
        let name = name.trim();
        if !name.is_empty() {
            self.name = name.to_string();
        }
    }
}

/// Remove only TUI-generated image attachment markers from the beginning of a
/// prompt. Other bracketed user text (`[workspace]`, Markdown, tags) is content.
fn strip_leading_image_markers(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim_start();
        let Some(marker) = trimmed.strip_prefix("[Image #") else {
            return trimmed;
        };
        let Some(close) = marker.find(']') else {
            return trimmed;
        };
        let number = &marker[..close];
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return trimmed;
        }
        text = &marker[close + 1..];
    }
}

/// Complete native session state for driver/API readers. Keeping the three
/// persisted artifacts together prevents consumers from inventing different
/// missing-file or ownership fallback rules.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSession {
    pub meta: SessionMeta,
    pub snapshot: SessionSnapshot,
    pub presentation: PresentationFile,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NativeSessionRepairOutcome {
    Healthy(LoadedSession),
    RepairableMissingPresentation {
        meta: SessionMeta,
        snapshot: SessionSnapshot,
    },
    Repaired(LoadedSession),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct InflightSnapshot {
    version: u32,
    replay_safe: bool,
    pub(super) snapshot: SessionSnapshot,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NativeImportCommitOutcome {
    Committed(SessionMeta),
    Conflict {
        meta: SessionMeta,
        snapshot: Option<SessionSnapshot>,
        presentation: Option<PresentationFile>,
    },
}

/// Minimal, core-free view of a historical `<id>.json`. Unknown fields, including
/// the full conversation, are streamed past by serde instead of entering catalog memory.
#[derive(Deserialize)]
struct LegacyCatalogMeta {
    id: String,
    name: String,
    working_dir: PathBuf,
    created_at: u64,
    updated_at: u64,
    messages: Vec<IgnoredAny>,
    #[serde(default)]
    turn_stats: Vec<IgnoredAny>,
}

#[derive(Default)]
struct CatalogAggregate {
    native: Option<SessionMeta>,
    legacy: Option<LegacyCatalogMeta>,
}

/// One completed turn's stats — drives a resume-time `✓ … tokens` divider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStat {
    /// Legacy mutable message-position anchor. Kept during the transition for old
    /// metadata/readers; native ownership and pruning decisions use `turn_id`.
    #[serde(default)]
    pub after_message: usize,
    /// Whether `after_message` belongs to the authoritative native snapshot's
    /// coordinate space. Metadata-only legacy imports retain accounting data but
    /// set this false because their offsets came from a different message history.
    #[serde(default = "default_true")]
    pub position_valid: bool,
    /// Stable kernel turn identity. `0` means an old meta that predates S1d and must
    /// be converted by the compatibility importer before native cutover.
    #[serde(default)]
    pub turn_id: u64,
    /// LLM round-trips within this completed user turn. Named `round_count` to avoid
    /// confusing it with [`SessionMeta::turn_count`] (completed user turns).
    #[serde(default)]
    pub round_count: u32,
    pub tool_call_count: u32,
    pub duration_ms: u64,
    /// Prompt + completion tokens from the final model request in this turn — the
    /// same value the live turn divider displays.
    pub total_tokens: u32,
    #[serde(default)]
    pub errored: bool,
    /// Prompt/context occupancy reported by the final model request.
    #[serde(default)]
    pub used_tokens: u32,
    /// Model context-window size paired with `used_tokens`.
    #[serde(default)]
    pub ctx_window: u32,
    /// Per-model usage produced during this turn. Empty for metadata written
    /// before model attribution was introduced; readers must keep that legacy
    /// total under "unattributed" rather than assigning it to the active model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_usage: Vec<ModelUsageStat>,
}

fn default_true() -> bool {
    true
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenBreakdown {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cached_input: u64,
}

impl TokenBreakdown {
    pub fn total(self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cached_input)
    }

    fn add_assign(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cached_input = self.cached_input.saturating_add(other.cached_input);
    }

    fn sub_assign(&mut self, other: Self) {
        self.input = self.input.saturating_sub(other.input);
        self.output = self.output.saturating_sub(other.output);
        self.cached_input = self.cached_input.saturating_sub(other.cached_input);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsageStat {
    pub provider_id: String,
    pub model_id: String,
    pub tokens: TokenBreakdown,
}

/// Session-scoped writer for model calls that run outside the primary agent
/// lifecycle hooks. It uses the metadata lock for cross-task/process safety and
/// deliberately does not mutate turn/message counters.
#[derive(Clone)]
pub struct DetachedUsageRecorder {
    manager: Arc<SessionManager>,
    session_id: String,
    provider_id: String,
    model_id: String,
    persistence_status: Option<super::snapshot::SnapshotPersistenceStatus>,
}

impl DetachedUsageRecorder {
    pub fn new(
        manager: Arc<SessionManager>,
        session_id: impl Into<String>,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            manager,
            session_id: session_id.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            persistence_status: None,
        }
    }

    pub fn with_persistence_status(
        mut self,
        status: super::snapshot::SnapshotPersistenceStatus,
    ) -> Self {
        self.persistence_status = Some(status);
        self
    }

    pub fn record(&self, tokens: TokenBreakdown) -> SessionResult<()> {
        if tokens.total() == 0 {
            return Ok(());
        }
        let result = self.manager.update_meta(&self.session_id, |meta| {
            merge_model_usage(
                &mut meta.detached_model_usage,
                ModelUsageStat {
                    provider_id: self.provider_id.clone(),
                    model_id: self.model_id.clone(),
                    tokens,
                },
            );
        });
        if let Err(error) = &result {
            if let Some(status) = &self.persistence_status {
                status.report_cost_warning(format!(
                    "model usage could not be persisted; /cost may be incomplete: {error}"
                ));
            }
        }
        result
    }
}

fn merge_model_usage(records: &mut Vec<ModelUsageStat>, usage: ModelUsageStat) {
    if let Some(existing) = records.iter_mut().find(|existing| {
        existing.provider_id == usage.provider_id && existing.model_id == usage.model_id
    }) {
        existing.tokens.add_assign(usage.tokens);
    } else {
        records.push(usage);
    }
}

fn subtract_model_usage(records: &mut Vec<ModelUsageStat>, usage: &ModelUsageStat) {
    if let Some(existing) = records.iter_mut().find(|existing| {
        existing.provider_id == usage.provider_id && existing.model_id == usage.model_id
    }) {
        existing.tokens.sub_assign(usage.tokens);
    }
    records.retain(|record| record.tokens.total() > 0);
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelCostSummary {
    pub provider_id: String,
    pub model_id: String,
    pub tokens: TokenBreakdown,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionCostReport {
    pub models: Vec<ModelCostSummary>,
    pub unattributed_tokens: u64,
    pub total_tokens: u64,
}

pub fn aggregate_session_cost(meta: &SessionMeta) -> SessionCostReport {
    use std::collections::BTreeMap;

    let mut grouped: BTreeMap<(String, String), TokenBreakdown> = BTreeMap::new();
    let mut unattributed_tokens = meta.detached_unattributed_tokens;

    let mut add_usage = |usage: &ModelUsageStat| {
        let entry = grouped
            .entry((usage.provider_id.clone(), usage.model_id.clone()))
            .or_default();
        entry.add_assign(usage.tokens);
    };

    for turn in &meta.turn_stats {
        if turn.model_usage.is_empty() {
            unattributed_tokens = unattributed_tokens.saturating_add(u64::from(turn.total_tokens));
            continue;
        }
        for usage in &turn.model_usage {
            add_usage(usage);
        }
    }
    for usage in &meta.detached_model_usage {
        add_usage(usage);
    }

    let models: Vec<_> = grouped
        .into_iter()
        .map(|((provider_id, model_id), tokens)| ModelCostSummary {
            provider_id,
            model_id,
            tokens,
        })
        .collect();
    let attributed_total = models.iter().fold(0_u64, |total, model| {
        total.saturating_add(model.tokens.total())
    });
    let total_tokens = attributed_total.saturating_add(unattributed_tokens);
    SessionCostReport {
        models,
        unattributed_tokens,
        total_tokens,
    }
}

/// The per-project session store at `$ATOMCODE_HOME/sessions/<project_hash>/`.
pub struct SessionManager {
    root: PathBuf,
    #[cfg(test)]
    meta_read_pause: Mutex<Option<Arc<MetaReadPause>>>,
    #[cfg(test)]
    commit_write_faults: Mutex<VecDeque<CommitWriteFault>>,
    #[cfg(test)]
    commit_write_log: Mutex<Vec<CommitArtifact>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitArtifact {
    Snapshot,
    Presentation,
    Meta,
}

struct CommitReplacement {
    artifact: CommitArtifact,
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitFaultTiming {
    BeforeReplace,
    AfterReplace,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommitWriteFault {
    artifact: CommitArtifact,
    timing: CommitFaultTiming,
}

/// A cloneable RAII claim on one active session. The OS releases the advisory
/// lock when the last clone closes, including after process termination.
#[derive(Clone)]
pub struct SessionLease {
    inner: Arc<SessionLeaseInner>,
}

impl SessionLease {
    pub fn id(&self) -> &str {
        &self.inner.id
    }
}

impl fmt::Debug for SessionLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionLease")
            .field("id", &self.inner.id)
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

struct SessionLeaseInner {
    id: String,
    path: PathBuf,
    file: File,
}

impl Drop for SessionLeaseInner {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
pub(crate) struct MetaReadPause {
    entered: Barrier,
    resume: Barrier,
}

#[cfg(test)]
impl MetaReadPause {
    pub(crate) fn wait_until_read(&self) {
        self.entered.wait();
    }

    pub(crate) fn resume(&self) {
        self.resume.wait();
    }
}

impl SessionManager {
    pub fn sessions_root() -> PathBuf {
        super::config_dir().join("sessions")
    }

    /// Copy the pre-v4.16 macOS session tree into the canonical sessions root.
    /// An initialized canonical root is never modified.
    pub fn migrate_from_legacy() -> SessionResult<usize> {
        #[cfg(target_os = "macos")]
        {
            let Some(legacy_root) =
                dirs::data_local_dir().map(|path| path.join("atomcode").join("sessions"))
            else {
                return Ok(0);
            };
            migrate_sessions_from(&legacy_root, &Self::sessions_root())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(0)
        }
    }

    /// The store for `working_dir`'s project — `$ATOMCODE_HOME/sessions/<project_hash>`,
    /// the SAME bucket production uses (so old `<id>.json` and new `<id>.snapshot`
    /// sessions of the same project land together).
    pub fn for_project(working_dir: &Path) -> Self {
        let root = Self::sessions_root().join(Self::project_hash(working_dir));
        Self {
            root,
            #[cfg(test)]
            meta_read_pause: Mutex::new(None),
            #[cfg(test)]
            commit_write_faults: Mutex::new(VecDeque::new()),
            #[cfg(test)]
            commit_write_log: Mutex::new(Vec::new()),
        }
    }

    /// Point the store at an explicit directory (tests / custom layouts).
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            #[cfg(test)]
            meta_read_pause: Mutex::new(None),
            #[cfg(test)]
            commit_write_faults: Mutex::new(VecDeque::new()),
            #[cfg(test)]
            commit_write_log: Mutex::new(Vec::new()),
        }
    }

    /// The store's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stable per-project bucket id shared with project-scoped trust storage.
    pub fn project_hash(working_dir: &Path) -> String {
        atomcode_config::util::stable_project_hash(working_dir)
    }

    pub fn snapshot_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "snapshot")
    }
    /// Directory under which per-session tool-output artifacts are stored.
    /// Sibling of the snapshot: `<root>/<id>.artifacts/`.
    pub fn artifacts_dir(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "artifacts")
    }
    pub fn meta_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "meta")
    }
    fn meta_lock_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "meta.lock")
    }
    fn lease_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "lease")
    }
    /// The append-only transcript path the [`TranscriptHook`](super::TranscriptHook)
    /// writes (and the recall tool reads).
    pub fn jsonl_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "jsonl")
    }

    /// UI-only replay data. This file is intentionally separate from the runtime
    /// snapshot so display-only entries can never enter provider context.
    pub fn presentation_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "ui.json")
    }

    fn rewind_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "rewind.json")
    }

    fn rewind_transaction_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "rewind.txn.json")
    }

    pub(crate) fn load_rewind_ledger(
        &self,
        id: &str,
    ) -> SessionResult<super::rewind::RewindLedger> {
        let path = self.rewind_path(id)?;
        match read_regular_file_bounded(&path, "rewind ledger", MAX_META_BYTES) {
            Ok(bytes) => {
                let ledger: super::rewind::RewindLedger =
                    serde_json::from_slice(&bytes).map_err(|source| {
                        SessionStoreError::Corrupt {
                            kind: "rewind ledger",
                            message: format!("{}: {source}", path.display()),
                        }
                    })?;
                ledger
                    .validate()
                    .map_err(|message| SessionStoreError::Corrupt {
                        kind: "rewind ledger",
                        message: format!("{}: {message}", path.display()),
                    })?;
                Ok(ledger)
            }
            Err(SessionStoreError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(super::rewind::RewindLedger {
                    version: super::rewind::LEDGER_VERSION,
                    points: Vec::new(),
                })
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn save_rewind_ledger(
        &self,
        id: &str,
        ledger: &super::rewind::RewindLedger,
    ) -> SessionResult<()> {
        let bytes = serialize_bounded(ledger, "rewind ledger", MAX_META_BYTES)?;
        atomic_write(&self.rewind_path(id)?, &bytes)
    }

    pub(crate) fn save_rewind_ledger_with_lease(
        &self,
        lease: &SessionLease,
        ledger: &super::rewind::RewindLedger,
    ) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        self.save_rewind_ledger(lease.id(), ledger)
    }

    pub(crate) fn load_rewind_transaction(
        &self,
        id: &str,
    ) -> SessionResult<Option<super::rewind::RewindTransactionJournal>> {
        let path = self.rewind_transaction_path(id)?;
        match read_regular_file_bounded(&path, "rewind transaction", MAX_REWIND_TRANSACTION_BYTES) {
            Ok(bytes) => {
                let journal: super::rewind::RewindTransactionJournal =
                    serde_json::from_slice(&bytes).map_err(|source| {
                        SessionStoreError::Corrupt {
                            kind: "rewind transaction",
                            message: format!("{}: {source}", path.display()),
                        }
                    })?;
                journal
                    .validate()
                    .map_err(|message| SessionStoreError::Corrupt {
                        kind: "rewind transaction",
                        message: format!("{}: {message}", path.display()),
                    })?;
                Ok(Some(journal))
            }
            Err(SessionStoreError::NotFound { .. }) => Ok(None),
            Err(SessionStoreError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn save_rewind_transaction_with_lease(
        &self,
        lease: &SessionLease,
        journal: &super::rewind::RewindTransactionJournal,
    ) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        journal
            .validate()
            .map_err(|message| SessionStoreError::Corrupt {
                kind: "rewind transaction",
                message,
            })?;
        let bytes = serialize_bounded(journal, "rewind transaction", MAX_REWIND_TRANSACTION_BYTES)?;
        atomic_write(&self.rewind_transaction_path(lease.id())?, &bytes)
    }

    pub(crate) fn clear_rewind_transaction_with_lease(
        &self,
        lease: &SessionLease,
    ) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        let path = self.rewind_transaction_path(lease.id())?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_at(&path, error)),
        }
    }

    pub fn legacy_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "json")
    }

    /// Separate recovery checkpoint written after a user prompt is accepted.
    /// It is never part of the authoritative native aggregate.
    fn inflight_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "snapshot.inflight")
    }

    /// Save an inflight checkpoint. The independent file does not take the meta
    /// lock; callers must only write states that are safe to replay.
    pub(crate) fn save_inflight_snapshot(
        &self,
        id: &str,
        snap: &SessionSnapshot,
        replay_safe: bool,
    ) -> SessionResult<()> {
        validate_snapshot(snap)?;
        let checkpoint = InflightSnapshot {
            version: INFLIGHT_SNAPSHOT_VERSION,
            replay_safe,
            snapshot: snap.clone(),
        };
        let bytes = serialize_bounded(&checkpoint, "inflight snapshot", MAX_SNAPSHOT_BYTES)?;
        // No meta lock — the inflight file is independent of the canonical
        // snapshot/meta/presentation aggregate. Catalog readers may use only its
        // validated existence as a visibility signal; its contents never replace
        // canonical metadata in a list projection.
        atomic_write(&self.inflight_path(id)?, &bytes)
    }

    pub(crate) fn save_inflight_snapshot_with_lease(
        &self,
        lease: &SessionLease,
        snap: &SessionSnapshot,
        replay_safe: bool,
    ) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        self.save_inflight_snapshot(lease.id(), snap, replay_safe)
    }

    /// Load the auxiliary checkpoint without changing canonical load semantics.
    pub(crate) fn load_inflight_snapshot(
        &self,
        id: &str,
    ) -> SessionResult<Option<InflightSnapshot>> {
        let path = self.inflight_path(id)?;
        match read_regular_file_bounded(&path, "inflight snapshot", MAX_SNAPSHOT_BYTES) {
            Ok(bytes) => {
                let checkpoint: InflightSnapshot = deserialize(&bytes, "inflight snapshot")?;
                if checkpoint.version != INFLIGHT_SNAPSHOT_VERSION {
                    return Err(SessionStoreError::Corrupt {
                        kind: "inflight snapshot",
                        message: format!(
                            "unsupported version {} (expected {})",
                            checkpoint.version, INFLIGHT_SNAPSHOT_VERSION
                        ),
                    });
                }
                validate_snapshot(&checkpoint.snapshot)?;
                Ok(Some(checkpoint))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Remove the inflight snapshot file. Called on a clean `turn_complete` so
    /// the next resume doesn't see a stale inflight from a turn that finished
    /// normally. Best-effort: a missing file is not an error.
    pub(crate) fn clear_inflight_snapshot(&self, id: &str) {
        if let Ok(path) = self.inflight_path(id) {
            let _ = fs::remove_file(path);
        }
    }

    pub(crate) fn mark_inflight_not_replayable(&self, id: &str) -> SessionResult<()> {
        let Some(mut checkpoint) = self.load_inflight_snapshot(id)? else {
            return Ok(());
        };
        if checkpoint.replay_safe {
            checkpoint.replay_safe = false;
            let bytes = serialize_bounded(&checkpoint, "inflight snapshot", MAX_SNAPSHOT_BYTES)?;
            atomic_write(&self.inflight_path(id)?, &bytes)?;
        }
        Ok(())
    }

    /// Whether `id` has a well-formed inflight checkpoint.
    ///
    /// This is a read-only driver/catalog projection seam. It does not imply the
    /// checkpoint is replay-safe: once model processing starts, the checkpoint
    /// remains useful evidence that a zero-count session has accepted work even
    /// though automatic replay may be disabled.
    pub fn has_valid_inflight_snapshot(&self, id: &str) -> bool {
        matches!(self.load_inflight_snapshot(id), Ok(Some(_)))
    }

    /// Test-only raw existence check used by cleanup/failure-path assertions.
    #[cfg(test)]
    pub(crate) fn has_inflight_snapshot(&self, id: &str) -> bool {
        self.inflight_path(id).map(|p| p.exists()).unwrap_or(false)
    }

    /// Load a native session for runtime resume and recover one accepted user
    /// prompt when a prior process died before reaching `turn_complete`.
    ///
    /// Recovery requires the active runtime lease and only accepts an inflight
    /// snapshot that is exactly the canonical message prefix plus one final user
    /// message. This rejects stale checkpoints and incomplete assistant/tool
    /// rounds. General readers continue to use [`Self::load_native_session`] and
    /// always receive the committed native aggregate.
    pub fn load_native_session_for_resume(
        &self,
        lease: &SessionLease,
    ) -> SessionResult<(LoadedSession, Option<Message>)> {
        self.validate_active_lease(lease)?;
        let mut loaded = self.load_native_session(lease.id())?;
        let inflight = match self.load_inflight_snapshot(lease.id()) {
            Ok(Some(checkpoint)) => checkpoint,
            Ok(None) => return Ok((loaded, None)),
            Err(error) => {
                eprintln!(
                    "[SessionManager] ignoring unreadable inflight snapshot for {}: {error}",
                    lease.id()
                );
                self.clear_inflight_snapshot(lease.id());
                return Ok((loaded, None));
            }
        };
        let canonical_len = loaded.snapshot.messages.len();
        let recoverable = inflight.snapshot.messages.len() == canonical_len.saturating_add(1)
            && inflight.snapshot.messages[..canonical_len] == loaded.snapshot.messages
            && inflight.snapshot.messages.last().is_some_and(|message| {
                message.role == atomcode_kernel::message::Role::User
                    && !message.synthetic
                    && message.internal_origin.is_none()
                    && message.tool_calls.is_empty()
            });
        if recoverable {
            if inflight.replay_safe {
                Ok((loaded, inflight.snapshot.messages.last().cloned()))
            } else {
                loaded.snapshot = inflight.snapshot;
                Ok((loaded, None))
            }
        } else {
            // A stale or unsafe auxiliary checkpoint must not shadow committed
            // state on future resumes.
            self.clear_inflight_snapshot(lease.id());
            Ok((loaded, None))
        }
    }

    /// Read a historical core session under the same no-follow and size bounds as
    /// catalog discovery. Parsing stays in the daemon compatibility layer.
    pub fn read_legacy_bytes(&self, id: &str) -> SessionResult<Vec<u8>> {
        read_regular_file_bounded(
            &self.legacy_path(id)?,
            "legacy session",
            MAX_LEGACY_SESSION_BYTES,
        )
    }

    fn path_for(&self, id: &str, extension: &str) -> SessionResult<PathBuf> {
        validate_session_id(id)?;
        Ok(self.root.join(format!("{id}.{extension}")))
    }

    /// Try to become the sole active runtime for `id`. Contention is reported
    /// immediately as [`SessionStoreError::SessionInUse`]; this never waits.
    pub fn acquire_lease(&self, id: &str) -> SessionResult<SessionLease> {
        validate_session_id(id)?;
        fs::create_dir_all(&self.root).map_err(|error| io_at(&self.root, error))?;
        let path = self.lease_path(id)?;
        reject_existing_non_regular(&path)?;
        let file = open_session_lock_file(id, &path)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(SessionLease {
                inner: Arc::new(SessionLeaseInner {
                    id: id.to_string(),
                    path,
                    file,
                }),
            }),
            Err(error) if is_file_lock_contention(&error) => Err(SessionStoreError::SessionInUse {
                id: id.to_string(),
                path,
            }),
            Err(error) => Err(io_at(&path, error)),
        }
    }

    /// Persist the working-set snapshot (atomic). Overwrites every turn.
    pub fn save_snapshot(&self, id: &str, snap: &SessionSnapshot) -> SessionResult<()> {
        validate_snapshot(snap)?;
        let bytes = serialize_bounded(snap, "snapshot", MAX_SNAPSHOT_BYTES)?;
        self.with_meta_lock(id, || {
            self.ensure_native_writable(id, "save snapshot")?;
            atomic_write(&self.snapshot_path(id)?, &bytes)
        })
    }

    pub fn load_snapshot(&self, id: &str) -> SessionResult<SessionSnapshot> {
        let bytes =
            read_regular_file_bounded(&self.snapshot_path(id)?, "snapshot", MAX_SNAPSHOT_BYTES)?;
        let snapshot: SessionSnapshot = deserialize(&bytes, "snapshot")?;
        validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    pub fn write_meta(&self, meta: &SessionMeta) -> SessionResult<()> {
        self.with_meta_lock(&meta.id, || {
            match self.read_meta(&meta.id) {
                Ok(existing) if existing.owner != meta.owner => {
                    return Err(SessionStoreError::OwnershipConflict {
                        id: meta.id.clone(),
                        owner: existing.owner,
                        operation: "change storage owner outside importer commit",
                    });
                }
                Ok(existing) if existing.owner == StorageOwner::Legacy => {
                    return Err(SessionStoreError::OwnershipConflict {
                        id: meta.id.clone(),
                        owner: existing.owner,
                        operation: "write native metadata",
                    });
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            self.write_meta_unlocked(meta)
        })
    }

    fn write_meta_unlocked(&self, meta: &SessionMeta) -> SessionResult<()> {
        validate_meta(meta)?;
        let bytes = serialize_pretty_bounded(meta, "session meta", MAX_META_BYTES)?;
        atomic_write(&self.meta_path(&meta.id)?, &bytes)
    }

    /// Atomically mutate an existing meta across threads and processes. The advisory
    /// lock covers the whole read-modify-write sequence; `write_meta` alone only
    /// serializes complete replacements and must not be used with a stale prior read.
    pub fn update_meta<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut SessionMeta) -> T,
    ) -> SessionResult<T> {
        self.with_meta_lock(id, || {
            let mut meta = self.read_meta(id)?;
            if meta.owner == StorageOwner::Legacy {
                return Err(SessionStoreError::OwnershipConflict {
                    id: id.to_string(),
                    owner: StorageOwner::Legacy,
                    operation: "update native metadata",
                });
            }
            let original_owner = meta.owner.clone();
            let result = update(&mut meta);
            ensure_meta_id(id, &meta)?;
            if meta.owner != original_owner {
                return Err(SessionStoreError::OwnershipConflict {
                    id: id.to_string(),
                    owner: meta.owner,
                    operation: "change storage owner through metadata update",
                });
            }
            self.write_meta_unlocked(&meta)?;
            Ok(result)
        })
    }

    pub(crate) fn update_meta_or_insert(
        &self,
        id: &str,
        new_meta: SessionMeta,
        update: impl FnOnce(&mut SessionMeta),
    ) -> SessionResult<()> {
        self.with_meta_lock(id, || {
            let mut meta = match self.read_meta(id) {
                Ok(meta) => meta,
                Err(error) if error.kind() == io::ErrorKind::NotFound => new_meta,
                Err(error) => return Err(error),
            };
            if meta.owner == StorageOwner::Legacy {
                return Err(SessionStoreError::OwnershipConflict {
                    id: id.to_string(),
                    owner: StorageOwner::Legacy,
                    operation: "update native metadata",
                });
            }
            update(&mut meta);
            ensure_meta_id(id, &meta)?;
            self.write_meta_unlocked(&meta)
        })
    }

    fn with_meta_lock<T>(
        &self,
        id: &str,
        operation: impl FnOnce() -> SessionResult<T>,
    ) -> SessionResult<T> {
        validate_session_id(id)?;
        fs::create_dir_all(&self.root).map_err(|e| io_at(&self.root, e))?;
        let lock_path = self.meta_lock_path(id)?;
        reject_existing_non_regular(&lock_path)?;
        let lock = open_session_lock_file(id, &lock_path)?;
        acquire_file_lock_until(&lock, META_LOCK_WAIT_TIMEOUT).map_err(|error| {
            if is_file_lock_contention(&error) {
                SessionStoreError::SessionInUse {
                    id: id.to_string(),
                    path: lock_path.clone(),
                }
            } else {
                io_at(&lock_path, error)
            }
        })?;
        operation()
    }

    fn commit_atomic_write(
        &self,
        _artifact: CommitArtifact,
        path: &Path,
        bytes: &[u8],
    ) -> SessionResult<()> {
        #[cfg(test)]
        {
            self.commit_write_log.lock().unwrap().push(_artifact);
            if self.take_commit_write_fault(_artifact, CommitFaultTiming::BeforeReplace) {
                return Err(io_at(
                    path,
                    io::Error::other("injected commit failure before replacement"),
                ));
            }
        }
        atomic_write(path, bytes)?;
        #[cfg(test)]
        if self.take_commit_write_fault(_artifact, CommitFaultTiming::AfterReplace) {
            return Err(io_at(
                path,
                io::Error::other("injected commit failure after replacement"),
            ));
        }
        Ok(())
    }

    fn commit_replacements_with_rollback(
        &self,
        id: &str,
        replacements: &[CommitReplacement],
    ) -> SessionResult<()> {
        let mut attempted = Vec::with_capacity(replacements.len());
        for (index, replacement) in replacements.iter().enumerate() {
            attempted.push(index);
            if let Err(commit_error) = self.commit_atomic_write(
                replacement.artifact,
                &replacement.path,
                &replacement.after,
            ) {
                let mut rollback_errors = Vec::new();
                // Restore the catalog-visible metadata commit point first, then
                // unwind sidecars in reverse publication order.
                for attempted_index in attempted.into_iter().rev() {
                    let attempted_replacement = &replacements[attempted_index];
                    let rollback = match attempted_replacement.before.as_deref() {
                        Some(before) => self.commit_atomic_write(
                            attempted_replacement.artifact,
                            &attempted_replacement.path,
                            before,
                        ),
                        None => self.commit_atomic_remove(
                            attempted_replacement.artifact,
                            &attempted_replacement.path,
                        ),
                    };
                    if let Err(rollback_error) = rollback {
                        rollback_errors.push(format!(
                            "{}: {rollback_error}",
                            attempted_replacement.path.display()
                        ));
                    }
                }
                if rollback_errors.is_empty() {
                    return Err(commit_error);
                }
                return Err(SessionStoreError::UncertainCommit {
                    id: id.to_string(),
                    commit_error: commit_error.to_string(),
                    rollback_errors,
                });
            }
        }
        Ok(())
    }

    fn commit_atomic_remove(&self, _artifact: CommitArtifact, path: &Path) -> SessionResult<()> {
        #[cfg(test)]
        {
            self.commit_write_log.lock().unwrap().push(_artifact);
            if self.take_commit_write_fault(_artifact, CommitFaultTiming::BeforeReplace) {
                return Err(io_at(
                    path,
                    io::Error::other("injected commit failure before removal"),
                ));
            }
        }
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_at(path, error)),
        }
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| io_at(parent, error))?;
        }
        #[cfg(test)]
        if self.take_commit_write_fault(_artifact, CommitFaultTiming::AfterReplace) {
            return Err(io_at(
                path,
                io::Error::other("injected commit failure after removal"),
            ));
        }
        Ok(())
    }

    fn read_snapshot_artifact(&self, id: &str) -> SessionResult<(SessionSnapshot, Vec<u8>)> {
        let bytes =
            read_regular_file_bounded(&self.snapshot_path(id)?, "snapshot", MAX_SNAPSHOT_BYTES)?;
        let snapshot: SessionSnapshot = deserialize(&bytes, "snapshot")?;
        validate_snapshot(&snapshot)?;
        Ok((snapshot, bytes))
    }

    fn read_optional_snapshot_artifact(
        &self,
        id: &str,
    ) -> SessionResult<Option<(SessionSnapshot, Vec<u8>)>> {
        match self.read_snapshot_artifact(id) {
            Ok(artifact) => Ok(Some(artifact)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn read_presentation_artifact(&self, id: &str) -> SessionResult<(PresentationFile, Vec<u8>)> {
        let bytes = read_regular_file_bounded(
            &self.presentation_path(id)?,
            "presentation",
            MAX_PRESENTATION_BYTES,
        )?;
        let presentation: PresentationFile = deserialize(&bytes, "presentation")?;
        presentation.validate()?;
        Ok((presentation, bytes))
    }

    fn read_optional_presentation_artifact(
        &self,
        id: &str,
    ) -> SessionResult<Option<(PresentationFile, Vec<u8>)>> {
        match self.read_presentation_artifact(id) {
            Ok(artifact) => Ok(Some(artifact)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn read_meta_artifact(&self, id: &str) -> SessionResult<(SessionMeta, Vec<u8>)> {
        let bytes =
            read_regular_file_bounded(&self.meta_path(id)?, "session meta", MAX_META_BYTES)?;
        let meta: SessionMeta = deserialize(&bytes, "session meta")?;
        if meta.v > META_VERSION {
            return Err(SessionStoreError::FutureSchema {
                kind: "session meta",
                found: meta.v,
                supported: META_VERSION,
            });
        }
        validate_meta(&meta)?;
        ensure_meta_id(id, &meta)?;
        #[cfg(test)]
        if let Some(pause) = self.meta_read_pause.lock().unwrap().take() {
            pause.entered.wait();
            pause.resume.wait();
        }
        Ok((meta, bytes))
    }

    fn read_optional_meta_artifact(
        &self,
        id: &str,
    ) -> SessionResult<Option<(SessionMeta, Vec<u8>)>> {
        match self.read_meta_artifact(id) {
            Ok(artifact) => Ok(Some(artifact)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    fn take_commit_write_fault(&self, artifact: CommitArtifact, timing: CommitFaultTiming) -> bool {
        let mut faults = self.commit_write_faults.lock().unwrap();
        if faults
            .front()
            .is_some_and(|fault| fault.artifact == artifact && fault.timing == timing)
        {
            faults.pop_front();
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn fail_commit_write(&self, artifact: CommitArtifact, timing: CommitFaultTiming) {
        self.commit_write_faults
            .lock()
            .unwrap()
            .push_back(CommitWriteFault { artifact, timing });
    }

    #[cfg(test)]
    fn take_commit_write_log(&self) -> Vec<CommitArtifact> {
        std::mem::take(&mut *self.commit_write_log.lock().unwrap())
    }

    pub fn read_meta(&self, id: &str) -> SessionResult<SessionMeta> {
        let bytes =
            read_regular_file_bounded(&self.meta_path(id)?, "session meta", MAX_META_BYTES)?;
        let meta: SessionMeta = deserialize(&bytes, "session meta")?;
        // The forward-compat seam, READER-ENFORCED (same rule as the kernel's
        // SNAPSHOT_VERSION check): a file from a future breaking schema may still
        // deserialize under this layout — refuse rather than silently misinterpret.
        if meta.v > META_VERSION {
            return Err(SessionStoreError::FutureSchema {
                kind: "session meta",
                found: meta.v,
                supported: META_VERSION,
            });
        }
        validate_meta(&meta)?;
        if meta.id != id {
            return Err(SessionStoreError::Corrupt {
                kind: "session meta",
                message: format!("file id {id:?} does not match stored id {:?}", meta.id),
            });
        }
        #[cfg(test)]
        if let Some(pause) = self.meta_read_pause.lock().unwrap().take() {
            pause.entered.wait();
            pause.resume.wait();
        }
        Ok(meta)
    }

    pub fn write_presentation(
        &self,
        id: &str,
        presentation: &PresentationFile,
    ) -> SessionResult<()> {
        self.with_meta_lock(id, || {
            self.ensure_native_writable(id, "write presentation")?;
            self.write_presentation_unlocked(id, presentation)
        })
    }

    fn write_presentation_unlocked(
        &self,
        id: &str,
        presentation: &PresentationFile,
    ) -> SessionResult<()> {
        presentation.validate()?;
        let bytes = serialize_pretty_bounded(presentation, "presentation", MAX_PRESENTATION_BYTES)?;
        atomic_write(&self.presentation_path(id)?, &bytes)
    }

    pub fn read_presentation(&self, id: &str) -> SessionResult<PresentationFile> {
        let bytes = read_regular_file_bounded(
            &self.presentation_path(id)?,
            "presentation",
            MAX_PRESENTATION_BYTES,
        )?;
        let presentation: PresentationFile = deserialize(&bytes, "presentation")?;
        presentation.validate()?;
        Ok(presentation)
    }

    /// Load the complete authoritative native state. A legacy/unconfirmed owner or
    /// any missing artifact is an explicit error; callers must cut over through the
    /// importer instead of manufacturing defaults.
    pub fn load_native_session(&self, id: &str) -> SessionResult<LoadedSession> {
        self.with_meta_lock(id, || self.load_native_session_unlocked(id))
    }

    fn load_native_session_unlocked(&self, id: &str) -> SessionResult<LoadedSession> {
        let meta = self.read_meta(id)?;
        if meta.owner != StorageOwner::Native {
            return Err(SessionStoreError::OwnershipConflict {
                id: id.to_string(),
                owner: meta.owner,
                operation: "load native session",
            });
        }
        let snapshot = self.load_snapshot(id)?;
        let presentation = self.read_presentation(id)?;
        Ok(LoadedSession {
            meta,
            snapshot,
            presentation,
        })
    }

    /// Inspect or explicitly repair the one safe incomplete-native state: valid
    /// native metadata and snapshot with an absent presentation sidecar.
    ///
    /// Existing presentation bytes are never replaced. The caller must hold the
    /// active session lease so a runtime cannot publish a concurrent aggregate.
    pub fn repair_missing_presentation(
        &self,
        lease: &SessionLease,
        apply: bool,
    ) -> SessionResult<NativeSessionRepairOutcome> {
        self.validate_active_lease(lease)?;
        self.with_meta_lock(lease.id(), || {
            let id = lease.id();
            let meta = self.read_meta(id)?;
            if meta.owner != StorageOwner::Native {
                return Err(SessionStoreError::OwnershipConflict {
                    id: id.to_string(),
                    owner: meta.owner,
                    operation: "repair missing presentation",
                });
            }
            let snapshot = self.load_snapshot(id)?;
            let presentation_path = self.presentation_path(id)?;
            match self.read_presentation(id) {
                Ok(presentation) => Ok(NativeSessionRepairOutcome::Healthy(LoadedSession {
                    meta,
                    snapshot,
                    presentation,
                })),
                Err(SessionStoreError::NotFound { path }) if path == presentation_path => {
                    if !apply {
                        return Ok(NativeSessionRepairOutcome::RepairableMissingPresentation {
                            meta,
                            snapshot,
                        });
                    }
                    self.write_presentation_unlocked(id, &PresentationFile::default())?;
                    self.load_native_session_unlocked(id)
                        .map(NativeSessionRepairOutcome::Repaired)
                }
                Err(error) => Err(error),
            }
        })
    }

    /// Clone one complete committed native aggregate under a new identity.
    ///
    /// The source may be owned by another active runtime: readers take the
    /// short-lived metadata lock and observe one committed aggregate, while the
    /// source runtime keeps its independent lifetime lease. The destination is
    /// published under a new exclusive lease and returned to the caller for
    /// direct transfer into its runtime.
    pub fn fork_native_session(
        &self,
        source_id: &str,
        destination_id: &str,
        now_ms: i64,
    ) -> SessionResult<(LoadedSession, SessionLease)> {
        let source = self.load_native_session(source_id)?;
        self.reap_abandoned_forks(&source);
        let destination_lease = self.acquire_lease(destination_id)?;
        let source_name_was_placeholder =
            SessionMeta::name_needs_fallback(&source.meta.name, source_id);
        let root_id = source
            .meta
            .fork_info
            .as_ref()
            .map(|fork| fork.root_id.clone())
            .unwrap_or_else(|| source_id.to_string());
        let mut meta = source.meta.clone();
        meta.id = destination_id.to_string();
        meta.name = if source_name_was_placeholder {
            format!("session-{destination_id}")
        } else {
            source.meta.name.clone()
        };
        meta.created_at = now_ms;
        meta.updated_at = now_ms;
        meta.owner = StorageOwner::Native;
        meta.import_info = None;
        meta.fork_info = Some(ForkInfo {
            root_id,
            parent_id: source_id.to_string(),
            forked_at_ms: now_ms,
            base_message_count: source.meta.message_count,
            base_turn_count: source.meta.turn_count,
        });
        let forked = LoadedSession {
            meta,
            snapshot: source.snapshot,
            presentation: source.presentation,
        };
        self.commit_native_import(
            &destination_lease,
            Some(&forked.snapshot),
            Some(&forked.presentation),
            &forked.meta,
        )?;
        Ok((forked, destination_lease))
    }

    /// Best-effort GC for automatic forks that were created but never used.
    fn reap_abandoned_forks(&self, source: &LoadedSession) {
        let root_id = source
            .meta
            .fork_info
            .as_ref()
            .map(|fork| fork.root_id.as_str())
            .unwrap_or(source.meta.id.as_str());
        for candidate in self.list() {
            if candidate.id == source.meta.id || candidate.id == root_id {
                continue;
            }
            let Some(lineage) = candidate.fork_info.as_ref() else {
                continue;
            };
            if lineage.root_id != root_id
                || candidate.message_count != lineage.base_message_count
                || candidate.turn_count != lineage.base_turn_count
                || candidate.updated_at != lineage.forked_at_ms
            {
                continue;
            }
            let Ok(lease) = self.acquire_lease(&candidate.id) else {
                continue;
            };
            let Ok(loaded) = self.load_native_session(&candidate.id) else {
                continue;
            };
            let Some(confirmed) = loaded.meta.fork_info.as_ref() else {
                continue;
            };
            if confirmed != lineage
                || loaded.meta.message_count != confirmed.base_message_count
                || loaded.meta.turn_count != confirmed.base_turn_count
                || loaded.meta.updated_at != confirmed.forked_at_ms
                || !self.transcript_is_empty(&candidate.id)
            {
                continue;
            }
            let _ = self.delete(&lease);
        }
    }

    fn transcript_is_empty(&self, id: &str) -> bool {
        let Ok(path) = self.jsonl_path(id) else {
            return false;
        };
        match fs::symlink_metadata(path) {
            Ok(metadata) => metadata.file_type().is_file() && metadata.len() == 0,
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        }
    }

    /// Publish a prepared legacy import under the caller's active session lease.
    /// Every payload is validated/serialized before the first target changes and
    /// `meta(owner=native)` is always the final reader-visible commit point.
    /// `None` preserves an already-valid native artifact selected by the importer.
    pub fn commit_native_import(
        &self,
        lease: &SessionLease,
        snapshot: Option<&SessionSnapshot>,
        presentation: Option<&PresentationFile>,
        meta: &SessionMeta,
    ) -> SessionResult<()> {
        self.validate_lease(lease)?;
        if meta.id != lease.id() {
            return Err(SessionStoreError::Corrupt {
                kind: "session import",
                message: format!(
                    "lease id {:?} does not match imported meta id {:?}",
                    lease.id(),
                    meta.id
                ),
            });
        }
        if meta.owner != StorageOwner::Native {
            return Err(SessionStoreError::Corrupt {
                kind: "session import",
                message: "commit point requires owner=native".into(),
            });
        }

        let snapshot_bytes = snapshot
            .map(|snapshot| {
                validate_snapshot(snapshot)?;
                serialize_bounded(snapshot, "snapshot", MAX_SNAPSHOT_BYTES)
            })
            .transpose()?;
        let presentation_bytes = presentation
            .map(|presentation| {
                presentation.validate()?;
                serialize_pretty_bounded(presentation, "presentation", MAX_PRESENTATION_BYTES)
            })
            .transpose()?;
        validate_meta(meta)?;
        let meta_bytes = serialize_pretty_bounded(meta, "session meta", MAX_META_BYTES)?;

        self.with_meta_lock(lease.id(), || {
            self.cleanup_import_staging(lease.id())?;
            let current_meta = self.read_optional_meta_artifact(lease.id())?;
            match current_meta.as_ref().map(|(meta, _)| meta) {
                None => {
                    if snapshot.is_none() || presentation.is_none() {
                        return Err(SessionStoreError::Corrupt {
                            kind: "session import",
                            message: "a new native aggregate requires snapshot and presentation"
                                .into(),
                        });
                    }
                }
                Some(existing) if existing.owner == StorageOwner::Legacy => {}
                Some(existing) if existing.owner == StorageOwner::Native && existing == meta => {}
                Some(existing) => {
                    return Err(SessionStoreError::OwnershipConflict {
                        id: lease.id().to_string(),
                        owner: existing.owner.clone(),
                        operation: "replace import metadata without expected-state CAS",
                    });
                }
            }
            let replacing_legacy_sidecars = current_meta
                .as_ref()
                .is_some_and(|(meta, _)| meta.owner == StorageOwner::Legacy);
            let (current_snapshot, invalid_snapshot_preimage) =
                match self.read_optional_snapshot_artifact(lease.id()) {
                    Err(SessionStoreError::Corrupt { .. })
                        if replacing_legacy_sidecars && snapshot.is_some() =>
                    {
                        (
                            None,
                            Some(read_regular_file_bounded(
                                &self.snapshot_path(lease.id())?,
                                "snapshot",
                                MAX_SNAPSHOT_BYTES,
                            )?),
                        )
                    }
                    result => (result?, None),
                };
            let (current_presentation, invalid_presentation_preimage) =
                match self.read_optional_presentation_artifact(lease.id()) {
                    Err(SessionStoreError::Corrupt { .. })
                        if replacing_legacy_sidecars && presentation.is_some() =>
                    {
                        (
                            None,
                            Some(read_regular_file_bounded(
                                &self.presentation_path(lease.id())?,
                                "presentation",
                                MAX_PRESENTATION_BYTES,
                            )?),
                        )
                    }
                    result => (result?, None),
                };
            if current_meta
                .as_ref()
                .is_some_and(|(existing, _)| existing.owner == StorageOwner::Native)
                && (snapshot.is_some_and(|snapshot| {
                    current_snapshot.as_ref().map(|(current, _)| current) != Some(snapshot)
                }) || presentation.is_some_and(|presentation| {
                    current_presentation.as_ref().map(|(current, _)| current) != Some(presentation)
                }))
            {
                return Err(SessionStoreError::OwnershipConflict {
                    id: lease.id().to_string(),
                    owner: StorageOwner::Native,
                    operation: "replace native sidecars without expected-state CAS",
                });
            }
            self.publish_native_import_locked(
                lease.id(),
                current_meta,
                current_snapshot,
                current_presentation,
                invalid_snapshot_preimage,
                invalid_presentation_preimage,
                snapshot,
                snapshot_bytes.as_deref(),
                presentation,
                presentation_bytes.as_deref(),
                meta,
                &meta_bytes,
            )
        })
    }

    /// Publish an import only if all existing native artifacts still match the
    /// state observed by the importer. `None` in the expected sidecars means the
    /// file was absent, not "ignore this artifact". A conflict performs no writes
    /// and returns the complete fresh state so the caller can recompute safely.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_native_import_if_unchanged(
        &self,
        lease: &SessionLease,
        expected_meta: &SessionMeta,
        expected_snapshot: Option<&SessionSnapshot>,
        expected_presentation: Option<&PresentationFile>,
        snapshot: Option<&SessionSnapshot>,
        presentation: Option<&PresentationFile>,
        meta: &SessionMeta,
    ) -> SessionResult<NativeImportCommitOutcome> {
        self.validate_lease(lease)?;
        ensure_meta_id(lease.id(), expected_meta)?;
        ensure_meta_id(lease.id(), meta)?;
        if meta.owner != StorageOwner::Native {
            return Err(SessionStoreError::Corrupt {
                kind: "session import",
                message: "commit point requires owner=native".into(),
            });
        }
        if expected_meta.owner != StorageOwner::Unconfirmed {
            return Err(SessionStoreError::OwnershipConflict {
                id: lease.id().to_string(),
                owner: expected_meta.owner.clone(),
                operation: "commit native import through unconfirmed-state CAS",
            });
        }
        validate_meta(expected_meta)?;
        validate_meta(meta)?;
        if let Some(snapshot) = expected_snapshot {
            validate_snapshot(snapshot)?;
        }
        if let Some(presentation) = expected_presentation {
            presentation.validate()?;
        }
        let snapshot_bytes = snapshot
            .map(|snapshot| {
                validate_snapshot(snapshot)?;
                serialize_bounded(snapshot, "snapshot", MAX_SNAPSHOT_BYTES)
            })
            .transpose()?;
        let presentation_bytes = presentation
            .map(|presentation| {
                presentation.validate()?;
                serialize_pretty_bounded(presentation, "presentation", MAX_PRESENTATION_BYTES)
            })
            .transpose()?;
        let meta_bytes = serialize_pretty_bounded(meta, "session meta", MAX_META_BYTES)?;

        self.with_meta_lock(lease.id(), || {
            self.cleanup_import_staging(lease.id())?;
            let Some(current_meta) = self.read_optional_meta_artifact(lease.id())? else {
                return Err(SessionStoreError::NotFound {
                    path: self.meta_path(lease.id())?,
                });
            };
            let current_snapshot = self.read_optional_snapshot_artifact(lease.id())?;
            let current_presentation = self.read_optional_presentation_artifact(lease.id())?;
            if current_meta.0 != *expected_meta
                || current_snapshot.as_ref().map(|(snapshot, _)| snapshot) != expected_snapshot
                || current_presentation
                    .as_ref()
                    .map(|(presentation, _)| presentation)
                    != expected_presentation
            {
                return Ok(NativeImportCommitOutcome::Conflict {
                    meta: current_meta.0,
                    snapshot: current_snapshot.map(|(snapshot, _)| snapshot),
                    presentation: current_presentation.map(|(presentation, _)| presentation),
                });
            }
            self.publish_native_import_locked(
                lease.id(),
                Some(current_meta),
                current_snapshot,
                current_presentation,
                None,
                None,
                snapshot,
                snapshot_bytes.as_deref(),
                presentation,
                presentation_bytes.as_deref(),
                meta,
                &meta_bytes,
            )?;
            Ok(NativeImportCommitOutcome::Committed(meta.clone()))
        })
    }

    /// Replace a previously committed empty metadata-only import when every
    /// native artifact still exactly matches the state inspected by the
    /// compatibility importer. This is intentionally narrower than a general
    /// native overwrite: only an empty aggregate with metadata-only provenance
    /// may be replaced by a populated full import of the same legacy source.
    #[allow(clippy::too_many_arguments)]
    pub fn recover_empty_metadata_only_import_if_unchanged(
        &self,
        lease: &SessionLease,
        expected_meta: &SessionMeta,
        expected_snapshot: &SessionSnapshot,
        expected_presentation: &PresentationFile,
        snapshot: &SessionSnapshot,
        presentation: &PresentationFile,
        meta: &SessionMeta,
    ) -> SessionResult<NativeImportCommitOutcome> {
        self.validate_lease(lease)?;
        ensure_meta_id(lease.id(), expected_meta)?;
        ensure_meta_id(lease.id(), meta)?;
        let expected_import = expected_meta.import_info.as_ref();
        let replacement_import = meta.import_info.as_ref();
        let valid_recovery = expected_meta.owner == StorageOwner::Native
            && expected_meta.message_count == 0
            && expected_snapshot.messages.is_empty()
            && expected_presentation.entries.is_empty()
            && expected_import.is_some_and(|info| info.kind == ImportKind::MetadataOnly)
            && meta.owner == StorageOwner::Native
            && !snapshot.messages.is_empty()
            && replacement_import.is_some_and(|info| {
                info.kind == ImportKind::Full
                    && expected_import.is_some_and(|expected| {
                        expected.source_sha256 == info.source_sha256
                            && expected.legacy_schema == info.legacy_schema
                    })
            });
        if !valid_recovery {
            return Err(SessionStoreError::Corrupt {
                kind: "session import recovery",
                message: "recovery requires an empty metadata-only native aggregate and a \
                          populated full import of the same legacy source"
                    .into(),
            });
        }
        validate_meta(expected_meta)?;
        validate_snapshot(expected_snapshot)?;
        expected_presentation.validate()?;
        validate_meta(meta)?;
        validate_snapshot(snapshot)?;
        presentation.validate()?;
        let snapshot_bytes = serialize_bounded(snapshot, "snapshot", MAX_SNAPSHOT_BYTES)?;
        let presentation_bytes =
            serialize_pretty_bounded(presentation, "presentation", MAX_PRESENTATION_BYTES)?;
        let meta_bytes = serialize_pretty_bounded(meta, "session meta", MAX_META_BYTES)?;

        self.with_meta_lock(lease.id(), || {
            self.cleanup_import_staging(lease.id())?;
            let Some(current_meta) = self.read_optional_meta_artifact(lease.id())? else {
                return Err(SessionStoreError::NotFound {
                    path: self.meta_path(lease.id())?,
                });
            };
            let current_snapshot = self.read_optional_snapshot_artifact(lease.id())?;
            let current_presentation = self.read_optional_presentation_artifact(lease.id())?;
            if current_meta.0 != *expected_meta
                || current_snapshot.as_ref().map(|(value, _)| value) != Some(expected_snapshot)
                || current_presentation.as_ref().map(|(value, _)| value)
                    != Some(expected_presentation)
            {
                return Ok(NativeImportCommitOutcome::Conflict {
                    meta: current_meta.0,
                    snapshot: current_snapshot.map(|(value, _)| value),
                    presentation: current_presentation.map(|(value, _)| value),
                });
            }
            self.publish_native_import_locked(
                lease.id(),
                Some(current_meta),
                current_snapshot,
                current_presentation,
                None,
                None,
                Some(snapshot),
                Some(&snapshot_bytes),
                Some(presentation),
                Some(&presentation_bytes),
                meta,
                &meta_bytes,
            )?;
            Ok(NativeImportCommitOutcome::Committed(meta.clone()))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_native_import_locked(
        &self,
        id: &str,
        current_meta: Option<(SessionMeta, Vec<u8>)>,
        current_snapshot: Option<(SessionSnapshot, Vec<u8>)>,
        current_presentation: Option<(PresentationFile, Vec<u8>)>,
        invalid_snapshot_preimage: Option<Vec<u8>>,
        invalid_presentation_preimage: Option<Vec<u8>>,
        snapshot: Option<&SessionSnapshot>,
        snapshot_bytes: Option<&[u8]>,
        presentation: Option<&PresentationFile>,
        presentation_bytes: Option<&[u8]>,
        meta: &SessionMeta,
        meta_bytes: &[u8],
    ) -> SessionResult<()> {
        let final_snapshot = snapshot
            .or_else(|| current_snapshot.as_ref().map(|(snapshot, _)| snapshot))
            .ok_or_else(|| SessionStoreError::NotFound {
                path: self.snapshot_path(id).expect("validated session id"),
            })?;
        validate_snapshot(final_snapshot)?;
        let final_presentation = presentation
            .or_else(|| {
                current_presentation
                    .as_ref()
                    .map(|(presentation, _)| presentation)
            })
            .ok_or_else(|| SessionStoreError::NotFound {
                path: self.presentation_path(id).expect("validated session id"),
            })?;
        final_presentation.validate()?;

        let mut replacements = Vec::with_capacity(3);
        if let Some(snapshot) = snapshot {
            if current_snapshot.as_ref().map(|(current, _)| current) != Some(snapshot) {
                replacements.push(CommitReplacement {
                    artifact: CommitArtifact::Snapshot,
                    path: self.snapshot_path(id)?,
                    before: current_snapshot
                        .as_ref()
                        .map(|(_, bytes)| bytes.clone())
                        .or_else(|| invalid_snapshot_preimage.clone()),
                    after: snapshot_bytes
                        .expect("serialized supplied snapshot")
                        .to_vec(),
                });
            }
        }
        if let Some(presentation) = presentation {
            if current_presentation.as_ref().map(|(current, _)| current) != Some(presentation) {
                replacements.push(CommitReplacement {
                    artifact: CommitArtifact::Presentation,
                    path: self.presentation_path(id)?,
                    before: current_presentation
                        .as_ref()
                        .map(|(_, bytes)| bytes.clone())
                        .or_else(|| invalid_presentation_preimage.clone()),
                    after: presentation_bytes
                        .expect("serialized supplied presentation")
                        .to_vec(),
                });
            }
        }
        if current_meta.as_ref().map(|(current, _)| current) != Some(meta) {
            replacements.push(CommitReplacement {
                artifact: CommitArtifact::Meta,
                path: self.meta_path(id)?,
                before: current_meta.as_ref().map(|(_, bytes)| bytes.clone()),
                after: meta_bytes.to_vec(),
            });
        }
        self.commit_replacements_with_rollback(id, &replacements)
    }

    /// Publish the durable intent for a full legacy cutover before replacing any
    /// snapshot or presentation sidecar. A subsequent importer run can therefore
    /// distinguish an interrupted cutover from an owner-native session whose
    /// historical metadata is missing.
    pub fn begin_legacy_import(
        &self,
        lease: &SessionLease,
        intent_meta: &SessionMeta,
    ) -> SessionResult<()> {
        self.validate_lease(lease)?;
        ensure_meta_id(lease.id(), intent_meta)?;
        if intent_meta.owner != StorageOwner::Legacy {
            return Err(SessionStoreError::Corrupt {
                kind: "session import",
                message: "legacy import intent requires owner=legacy".into(),
            });
        }
        if intent_meta.import_info.is_some() {
            return Err(SessionStoreError::Corrupt {
                kind: "session import",
                message: "legacy import intent must not claim completed import provenance".into(),
            });
        }
        validate_meta(intent_meta)?;
        let meta_bytes = serialize_pretty_bounded(intent_meta, "session meta", MAX_META_BYTES)?;

        self.with_meta_lock(lease.id(), || {
            match self.read_meta(lease.id()) {
                Ok(existing) if existing.owner == StorageOwner::Native => {
                    return Err(SessionStoreError::OwnershipConflict {
                        id: lease.id().to_string(),
                        owner: StorageOwner::Native,
                        operation: "begin legacy import",
                    });
                }
                Ok(existing) if existing.owner == StorageOwner::Unconfirmed => {
                    return Err(SessionStoreError::OwnershipConflict {
                        id: lease.id().to_string(),
                        owner: StorageOwner::Unconfirmed,
                        operation: "begin legacy import without expected-state CAS",
                    });
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            atomic_write(&self.meta_path(lease.id())?, &meta_bytes)
        })
    }

    /// Publish a legacy import intent only if the complete pre-cutover aggregate
    /// still matches the state observed by the importer. `None` is an exact
    /// expectation that the artifact is absent. A conflict performs no writes.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_legacy_import_if_unchanged(
        &self,
        lease: &SessionLease,
        expected_meta: Option<&SessionMeta>,
        expected_snapshot: Option<&SessionSnapshot>,
        expected_presentation: Option<&PresentationFile>,
        intent_meta: &SessionMeta,
    ) -> SessionResult<bool> {
        self.validate_lease(lease)?;
        ensure_meta_id(lease.id(), intent_meta)?;
        if let Some(expected_meta) = expected_meta {
            ensure_meta_id(lease.id(), expected_meta)?;
            validate_meta(expected_meta)?;
            if expected_meta.owner != StorageOwner::Unconfirmed {
                return Err(SessionStoreError::OwnershipConflict {
                    id: lease.id().to_string(),
                    owner: expected_meta.owner.clone(),
                    operation: "begin legacy import with expected-state CAS",
                });
            }
        }
        if let Some(expected_snapshot) = expected_snapshot {
            validate_snapshot(expected_snapshot)?;
        }
        if let Some(expected_presentation) = expected_presentation {
            expected_presentation.validate()?;
        }
        if intent_meta.owner != StorageOwner::Legacy || intent_meta.import_info.is_some() {
            return Err(SessionStoreError::Corrupt {
                kind: "session import",
                message: "legacy import intent must use owner=legacy without completed provenance"
                    .into(),
            });
        }
        validate_meta(intent_meta)?;
        let intent_bytes = serialize_pretty_bounded(intent_meta, "session meta", MAX_META_BYTES)?;

        self.with_meta_lock(lease.id(), || {
            let current_meta = self.read_optional_meta_artifact(lease.id())?;
            let current_snapshot = self.read_optional_snapshot_artifact(lease.id())?;
            let current_presentation = self.read_optional_presentation_artifact(lease.id())?;
            if current_meta.as_ref().map(|(meta, _)| meta) != expected_meta
                || current_snapshot.as_ref().map(|(snapshot, _)| snapshot) != expected_snapshot
                || current_presentation
                    .as_ref()
                    .map(|(presentation, _)| presentation)
                    != expected_presentation
            {
                return Ok(false);
            }
            self.commit_atomic_write(
                CommitArtifact::Meta,
                &self.meta_path(lease.id())?,
                &intent_bytes,
            )?;
            Ok(true)
        })
    }

    /// Commit a prepared owner-native metadata repair only if metadata, snapshot,
    /// and presentation still match the state on which the repair was based. The
    /// sidecars are CAS evidence only and are never rewritten by this operation.
    pub fn commit_native_sidecar_repair_if_unchanged(
        &self,
        lease: &SessionLease,
        expected_meta: &SessionMeta,
        expected_snapshot: &SessionSnapshot,
        expected_presentation: &PresentationFile,
        repaired_meta: &SessionMeta,
    ) -> SessionResult<bool> {
        self.validate_lease(lease)?;
        ensure_meta_id(lease.id(), expected_meta)?;
        ensure_meta_id(lease.id(), repaired_meta)?;
        if expected_meta.owner != StorageOwner::Native
            || repaired_meta.owner != StorageOwner::Native
        {
            return Err(SessionStoreError::Corrupt {
                kind: "session sidecar repair",
                message: "sidecar repair requires owner=native metadata".into(),
            });
        }
        validate_meta(expected_meta)?;
        validate_meta(repaired_meta)?;
        validate_snapshot(expected_snapshot)?;
        expected_presentation.validate()?;
        let repaired_meta_bytes =
            serialize_pretty_bounded(repaired_meta, "session meta", MAX_META_BYTES)?;

        self.with_meta_lock(lease.id(), || {
            let current_meta = self.read_meta(lease.id())?;
            if current_meta.owner != StorageOwner::Native {
                return Err(SessionStoreError::OwnershipConflict {
                    id: lease.id().to_string(),
                    owner: current_meta.owner,
                    operation: "commit native sidecar repair",
                });
            }
            let current_snapshot = self.load_snapshot(lease.id())?;
            let current_presentation = self.read_presentation(lease.id())?;
            if &current_meta != expected_meta
                || &current_snapshot != expected_snapshot
                || &current_presentation != expected_presentation
            {
                return Ok(false);
            }
            if &current_meta != repaired_meta {
                atomic_write(&self.meta_path(lease.id())?, &repaired_meta_bytes)?;
            }
            Ok(true)
        })
    }

    /// Commit an owner-native runtime mutation under the active session lease.
    /// The snapshot is prepared before locking; metadata and presentation are then
    /// loaded and mutated under their shared cross-process lock so a stale caller
    /// cannot overwrite a concurrent rename or presentation append. All three
    /// payloads are validated and serialized before the first replacement;
    /// metadata is written last and is the catalog-visible commit point.
    pub fn commit_native_runtime_mutation<T>(
        &self,
        lease: &SessionLease,
        snapshot: &SessionSnapshot,
        mutate: impl FnOnce(
            &SessionSnapshot,
            &mut SessionMeta,
            &mut PresentationFile,
        ) -> SessionResult<T>,
    ) -> SessionResult<T> {
        self.validate_lease(lease)?;
        validate_snapshot(snapshot)?;
        let snapshot_bytes = serialize_bounded(snapshot, "snapshot", MAX_SNAPSHOT_BYTES)?;

        self.with_meta_lock(lease.id(), || {
            let (current_snapshot, original_snapshot_bytes) =
                self.read_snapshot_artifact(lease.id())?;
            let (mut meta, original_meta_bytes) = self.read_meta_artifact(lease.id())?;
            if meta.owner != StorageOwner::Native {
                return Err(SessionStoreError::OwnershipConflict {
                    id: lease.id().to_string(),
                    owner: meta.owner,
                    operation: "commit native runtime mutation",
                });
            }
            let (mut presentation, original_presentation_bytes) =
                self.read_presentation_artifact(lease.id())?;
            let original_meta = meta.clone();
            let original_presentation = presentation.clone();
            let result = mutate(&current_snapshot, &mut meta, &mut presentation)?;
            ensure_meta_id(lease.id(), &meta)?;
            if meta.owner != StorageOwner::Native {
                return Err(SessionStoreError::OwnershipConflict {
                    id: lease.id().to_string(),
                    owner: meta.owner,
                    operation: "commit native runtime mutation",
                });
            }
            validate_meta(&meta)?;
            presentation.validate()?;
            let presentation_bytes =
                serialize_pretty_bounded(&presentation, "presentation", MAX_PRESENTATION_BYTES)?;
            let meta_bytes = serialize_pretty_bounded(&meta, "session meta", MAX_META_BYTES)?;
            let mut replacements = Vec::with_capacity(3);
            if current_snapshot != *snapshot {
                replacements.push(CommitReplacement {
                    artifact: CommitArtifact::Snapshot,
                    path: self.snapshot_path(lease.id())?,
                    before: Some(original_snapshot_bytes),
                    after: snapshot_bytes,
                });
            }
            if original_presentation != presentation {
                replacements.push(CommitReplacement {
                    artifact: CommitArtifact::Presentation,
                    path: self.presentation_path(lease.id())?,
                    before: Some(original_presentation_bytes),
                    after: presentation_bytes,
                });
            }
            if original_meta != meta {
                replacements.push(CommitReplacement {
                    artifact: CommitArtifact::Meta,
                    path: self.meta_path(lease.id())?,
                    before: Some(original_meta_bytes),
                    after: meta_bytes,
                });
            }
            self.commit_replacements_with_rollback(lease.id(), &replacements)?;
            Ok(result)
        })
    }

    fn validate_lease(&self, lease: &SessionLease) -> SessionResult<()> {
        let expected = self.lease_path(lease.id())?;
        if lease.inner.path == expected {
            Ok(())
        } else {
            Err(SessionStoreError::LeaseMismatch {
                id: lease.id().to_string(),
                expected,
                actual: lease.inner.path.clone(),
            })
        }
    }

    /// Verify that a lease was acquired for this exact project bucket/session.
    /// Drivers use this before transferring an importer-held guard into a runtime.
    pub fn validate_active_lease(&self, lease: &SessionLease) -> SessionResult<()> {
        self.validate_lease(lease)
    }

    fn cleanup_import_staging(&self, id: &str) -> SessionResult<()> {
        let prefixes = [
            format!(".{id}.snapshot."),
            format!(".{id}.meta."),
            format!(".{id}.ui.json."),
        ];
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_at(&self.root, error)),
        };
        for entry in entries {
            let entry = entry.map_err(|error| io_at(&self.root, error))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.ends_with(".tmp") || !prefixes.iter().any(|prefix| name.starts_with(prefix)) {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| io_at(&path, error))?;
            if !metadata.file_type().is_file() {
                return Err(SessionStoreError::UnsafeFile {
                    path,
                    reason: "import staging residue is not a regular file",
                });
            }
            fs::remove_file(&path).map_err(|error| io_at(&path, error))?;
        }
        Ok(())
    }

    pub fn append_presentation(&self, id: &str, entry: PresentationEntry) -> SessionResult<()> {
        self.with_meta_lock(id, || {
            self.ensure_native_writable(id, "append presentation")?;
            let mut presentation = match self.read_presentation(id) {
                Ok(presentation) => presentation,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    PresentationFile::default()
                }
                Err(error) => return Err(error),
            };
            presentation.entries.push(entry);
            self.write_presentation_unlocked(id, &presentation)
        })
    }

    /// Append catalog/UI messages at the latest valid native turn boundary. Meta
    /// lookup, anchor selection and presentation read-modify-write are one locked
    /// operation so a concurrent importer repair cannot invalidate the anchor or
    /// overwrite the appended entries.
    pub fn append_presentation_at_latest_valid_turn(
        &self,
        id: &str,
        messages: &[(PresentationRole, String)],
    ) -> SessionResult<usize> {
        self.with_meta_lock(id, || {
            let meta = self.read_meta(id)?;
            if meta.owner != StorageOwner::Native {
                return Err(SessionStoreError::OwnershipConflict {
                    id: id.to_string(),
                    owner: meta.owner,
                    operation: "append native presentation",
                });
            }
            let anchor = meta
                .turn_stats
                .iter()
                .rev()
                .find(|stat| stat.position_valid && stat.turn_id != 0)
                .map(|stat| DisplayAnchor::AfterTurn {
                    turn_id: stat.turn_id,
                })
                .unwrap_or(DisplayAnchor::AtStart);
            let mut presentation = self.read_presentation(id)?;
            presentation
                .entries
                .extend(messages.iter().map(|(role, text)| PresentationEntry {
                    anchor,
                    role: *role,
                    text: text.clone(),
                }));
            self.write_presentation_unlocked(id, &presentation)?;
            (meta.message_count as usize)
                .checked_add(presentation.entries.len())
                .ok_or(SessionStoreError::TooLarge {
                    kind: "catalog message count",
                    limit: usize::MAX,
                    actual: usize::MAX,
                })
        })
    }

    pub fn prune_presentation(
        &self,
        id: &str,
        surviving_turn_ids: &BTreeSet<u64>,
    ) -> SessionResult<usize> {
        self.with_meta_lock(id, || {
            self.ensure_native_writable(id, "prune presentation")?;
            let mut presentation = self.read_presentation(id)?;
            let removed = presentation.retain_turns(surviving_turn_ids);
            if removed != 0 {
                self.write_presentation_unlocked(id, &presentation)?;
            }
            Ok(removed)
        })
    }

    /// Drop native turn stats whose stable turn no longer survives. Legacy stats
    /// (`turn_id == 0`) stay untouched until the compatibility importer can map them.
    pub fn prune_turn_stats(
        &self,
        id: &str,
        surviving_turn_ids: &BTreeSet<u64>,
    ) -> SessionResult<usize> {
        self.with_meta_lock(id, || {
            let mut meta = self.read_meta(id)?;
            if meta.owner == StorageOwner::Legacy {
                return Err(SessionStoreError::OwnershipConflict {
                    id: id.to_string(),
                    owner: StorageOwner::Legacy,
                    operation: "prune native turn stats",
                });
            }
            let before = meta.turn_stats.len();
            meta.turn_stats
                .retain(|stat| stat.turn_id == 0 || surviving_turn_ids.contains(&stat.turn_id));
            let removed = before - meta.turn_stats.len();
            if removed != 0 {
                self.write_meta_unlocked(&meta)?;
            }
            Ok(removed)
        })
    }

    #[cfg(test)]
    pub(crate) fn pause_next_meta_read(&self) -> Arc<MetaReadPause> {
        let pause = Arc::new(MetaReadPause {
            entered: Barrier::new(2),
            resume: Barrier::new(2),
        });
        *self.meta_read_pause.lock().unwrap() = Some(pause.clone());
        pause
    }

    /// Scan every project bucket below an explicit sessions root. Missing roots are
    /// an empty catalog; malformed individual entries become diagnostics.
    pub fn scan_catalog(sessions_root: &Path) -> CatalogScan {
        scan_catalog_cached(sessions_root)
    }

    /// Collapse automatic fork aggregates into one newest logical conversation
    /// row per project. Exact-ID loading and the raw catalog remain unchanged.
    pub fn collapse_fork_lineages(entries: &mut Vec<CatalogEntry>) {
        entries.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.project_bucket.cmp(&b.project_bucket))
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut seen = BTreeSet::new();
        entries.retain(|entry| {
            let logical_id = entry.fork_root_id.as_deref().unwrap_or(&entry.id);
            seen.insert((entry.project_bucket.clone(), logical_id.to_string()))
        });
    }

    pub fn scan_all() -> CatalogScan {
        Self::scan_catalog(&Self::sessions_root())
    }

    /// List all sessions in this project bucket, NEWEST FIRST. Reads ONLY `*.meta`
    /// (never the big snapshot / transcript files); a malformed meta is skipped, not
    /// fatal. Production's `<id>.json` files are ignored (different extension).
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(&self.root) else {
            return out;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("meta") {
                continue;
            }
            if let Ok(bytes) = read_regular_file_bounded(&path, "session meta", MAX_META_BYTES) {
                if let Ok(meta) = deserialize::<SessionMeta>(&bytes, "session meta") {
                    // Future-schema metas are skipped like malformed ones (reader-
                    // enforced version bound; see read_meta).
                    let file_id = path.file_stem().and_then(|stem| stem.to_str());
                    if meta.v <= META_VERSION
                        && validate_meta(&meta).is_ok()
                        && file_id == Some(meta.id.as_str())
                    {
                        out.push(meta);
                    }
                }
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    /// Sessions for normal pickers (/resume, webui sidebar): excludes scheduled-run
    /// sessions so recurring tasks don't flood the user's manual history. Use `list()`
    /// for the full set (e.g. a scheduled-tasks view).
    pub fn list_visible(&self) -> Vec<SessionMeta> {
        self.list()
            .into_iter()
            .filter(|m| m.origin != SessionOrigin::Scheduled)
            .collect()
    }

    /// The most-recently-updated session, if any.
    pub fn latest(&self) -> Option<SessionMeta> {
        self.list().into_iter().next()
    }

    /// Rename a session (sets `name` + `user_renamed`). Errors if no meta exists.
    pub fn rename(&self, id: &str, name: &str) -> SessionResult<()> {
        self.update_meta(id, |meta| {
            meta.name = name.to_string();
            meta.user_renamed = true;
        })
    }

    /// Remove native data plus the corresponding historical core JSON. The caller
    /// must already hold this exact bucket's active-session lease. Lock files remain
    /// persistent so no second inode can be locked while an old descriptor exists.
    pub fn delete(&self, lease: &SessionLease) -> SessionResult<()> {
        let id = lease.id();
        self.validate_lease(lease)?;
        let targets = [
            self.snapshot_path(id)?,
            self.inflight_path(id)?,
            self.rewind_path(id)?,
            self.rewind_transaction_path(id)?,
            self.meta_path(id)?,
            self.jsonl_path(id)?,
            self.presentation_path(id)?,
            self.legacy_path(id)?,
        ];
        for path in &targets {
            validate_delete_target(path)?;
        }
        for p in targets {
            match fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_at(&p, e)),
            }
        }
        if let Ok(dir) = self.artifacts_dir(id) {
            let _ = fs::remove_dir_all(&dir); // best-effort; absent dir is fine
        }
        Ok(())
    }

    pub(crate) fn append_jsonl_line(&self, id: &str, line: &[u8]) -> SessionResult<()> {
        self.ensure_native_writable(id, "append transcript")?;
        if line.len() > MAX_JSONL_LINE_BYTES {
            return Err(SessionStoreError::TooLarge {
                kind: "transcript line",
                limit: MAX_JSONL_LINE_BYTES,
                actual: line.len(),
            });
        }
        let path = self.jsonl_path(id)?;
        fs::create_dir_all(&self.root).map_err(|e| io_at(&self.root, e))?;
        // Windows security software and indexers can briefly deny an open or
        // lock while inspecting a newly-updated file. Retry only those
        // pre-write operations: retrying write_all itself could duplicate a
        // partially-written JSONL record.
        let mut file = retry_transient_file_access(|| open_append_file(&path))?;
        retry_transient_file_access(|| {
            fs2::FileExt::lock_exclusive(&file).map_err(|e| io_at(&path, e))
        })?;
        let current = usize::try_from(file.metadata().map_err(|e| io_at(&path, e))?.len())
            .unwrap_or(usize::MAX);
        let next = current
            .checked_add(line.len())
            .ok_or(SessionStoreError::TooLarge {
                kind: "transcript",
                limit: MAX_JSONL_BYTES,
                actual: usize::MAX,
            })?;
        if next > MAX_JSONL_BYTES {
            return Err(SessionStoreError::TooLarge {
                kind: "transcript",
                limit: MAX_JSONL_BYTES,
                actual: next,
            });
        }
        file.write_all(line).map_err(|e| io_at(&path, e))
    }

    /// Load only `(turn_id, timestamp_ms)` from this session's bounded transcript.
    /// Message bodies are streamed past instead of retained, so opening a large WebUI
    /// history does not duplicate the full append-only transcript in memory. A missing
    /// transcript is valid for imported or older sessions.
    pub fn load_transcript_timestamps(
        &self,
        id: &str,
    ) -> SessionResult<BTreeMap<u64, TurnTimestamp>> {
        #[derive(Deserialize)]
        struct TimestampRecord {
            #[serde(default)]
            v: u32,
            turn_id: u64,
            #[serde(default)]
            started_at: Option<i64>,
            ts: i64,
        }

        let path = self.jsonl_path(id)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(error) => return Err(io_at(&path, error)),
        }

        let mut timestamps = BTreeMap::new();
        for_each_jsonl_line(&path, |line| {
            let record: TimestampRecord =
                serde_json::from_slice(line).map_err(|error| SessionStoreError::Corrupt {
                    kind: "transcript record",
                    message: format!("{}: {error}", path.display()),
                })?;
            if record.v > RECORD_VERSION {
                return Err(SessionStoreError::FutureSchema {
                    kind: "transcript record",
                    found: record.v,
                    supported: RECORD_VERSION,
                });
            }
            timestamps.insert(
                record.turn_id,
                TurnTimestamp {
                    started_at: record.started_at,
                    completed_at: record.ts,
                },
            );
            Ok(())
        })?;
        Ok(timestamps)
    }

    fn ensure_native_writable(&self, id: &str, operation: &'static str) -> SessionResult<()> {
        match self.read_meta(id) {
            Ok(meta) if meta.owner == StorageOwner::Legacy => {
                Err(SessionStoreError::OwnershipConflict {
                    id: id.to_string(),
                    owner: StorageOwner::Legacy,
                    operation,
                })
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            // A corrupt historical meta cannot prove `owner=legacy`. Snapshot/
            // sidecar writes remain non-authoritative staging while the bad meta
            // stays in place; metadata mutation itself still fails closed.
            Err(SessionStoreError::Corrupt { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// One cached whole-root scan, keyed by `sessions_root`. `sig` is the cheap
/// directory fingerprint (see `catalog_signature`); when it matches, the scan
/// hasn't changed on disk and we can hand back the cached copy instead of
/// re-reading + re-parsing every `*.meta`. The scan is shared behind an `Arc`
/// so a hit clones the (small) `CatalogScan` once rather than the map value.
struct CachedCatalog {
    sig: u64,
    scan: Arc<CatalogScan>,
}

fn catalog_cache() -> &'static Mutex<HashMap<PathBuf, CachedCatalog>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedCatalog>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop the whole-root scan cache. Tests that mutate files under a root and
/// re-scan within the same second (below mtime resolution, same file size)
/// call this so the stale-detection edge case can't flake the assertion.
#[cfg(test)]
pub(crate) fn clear_catalog_cache() {
    catalog_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Cheap fingerprint of everything under `sessions_root` that could change the
/// catalog: for every file in every bucket, fold `(bucket, filename, mtime,
/// len)` into an order-independent accumulator. It `stat`s but never OPENS or
/// PARSES a file — that's the whole point (the full scan reads + JSON-parses
/// every `*.meta`). Any add / remove / rename / in-place rewrite (which bumps
/// mtime and/or len) changes the fingerprint, so the cache self-invalidates on
/// a change made by ANY process — no explicit invalidation wiring to forget.
fn catalog_signature(sessions_root: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut acc: u64 = 0;
    let Ok(buckets) = fs::read_dir(sessions_root) else {
        return 0;
    };
    for bucket in buckets.flatten() {
        if !bucket.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let bucket_name = bucket.file_name();
        let Ok(files) = fs::read_dir(bucket.path()) else {
            continue;
        };
        for file in files.flatten() {
            let Ok(md) = file.metadata() else {
                continue;
            };
            if !md.is_file() {
                continue;
            }
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bucket_name.hash(&mut h);
            file.file_name().hash(&mut h);
            mtime.hash(&mut h);
            md.len().hash(&mut h);
            // Commutative fold: read_dir order is not stable across calls, so
            // the combine step must not depend on iteration order.
            acc = acc.wrapping_add(h.finish());
        }
    }
    acc
}

/// Cache wrapper around [`scan_catalog_root`]. On a fingerprint hit, returns a
/// clone of the cached scan (skips reading + parsing every `*.meta`); on a
/// miss, does the real scan and stores it. Kept out of the daemon serialize
/// layer so every catalog consumer (TUI `/resume`, `/sessions`, search) shares
/// one cache. Callers are unchanged — this is a transparent speedup.
///
/// NOTE: a hit still pays the `catalog_signature` walk (O(N `stat`s), NOT O(1))
/// — it saves the read + JSON-parse of every `*.meta`, not the directory walk.
/// Index-level (single-read) speed would need a persisted recency index.
fn scan_catalog_cached(sessions_root: &Path) -> CatalogScan {
    let sig = catalog_signature(sessions_root);
    {
        let cache = catalog_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.get(sessions_root) {
            if cached.sig == sig {
                return (*cached.scan).clone();
            }
        }
    }
    // Miss: the lock is intentionally NOT held across the (slow) scan.
    let scan = scan_catalog_root(sessions_root);
    let arc = Arc::new(scan.clone());
    catalog_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            sessions_root.to_path_buf(),
            CachedCatalog { sig, scan: arc },
        );
    scan
}

fn scan_catalog_root(sessions_root: &Path) -> CatalogScan {
    let mut scan = CatalogScan::default();
    let mut sessions: BTreeMap<(String, String), CatalogAggregate> = BTreeMap::new();
    let mut native_meta_ids = BTreeSet::new();
    let mut native_sidecars = BTreeMap::new();
    let buckets = match fs::read_dir(sessions_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return scan,
        Err(error) => {
            scan.diagnostics.push(io_catalog_diagnostic(
                None,
                sessions_root.to_path_buf(),
                error,
            ));
            return scan;
        }
    };

    for bucket_result in buckets {
        let bucket_entry = match bucket_result {
            Ok(entry) => entry,
            Err(error) => {
                scan.diagnostics.push(io_catalog_diagnostic(
                    None,
                    sessions_root.to_path_buf(),
                    error,
                ));
                continue;
            }
        };
        let bucket_path = bucket_entry.path();
        let bucket = match bucket_entry.file_name().into_string() {
            Ok(bucket) if valid_project_bucket(&bucket) => bucket,
            Ok(bucket) => {
                scan.diagnostics.push(CatalogDiagnostic {
                    project_bucket: Some(bucket),
                    path: bucket_path,
                    kind: CatalogDiagnosticKind::InvalidId,
                    message: "project bucket must be exactly 16 ASCII hex characters".into(),
                });
                continue;
            }
            Err(_) => {
                scan.diagnostics.push(CatalogDiagnostic {
                    project_bucket: None,
                    path: bucket_path,
                    kind: CatalogDiagnosticKind::InvalidId,
                    message: "project bucket name is not valid UTF-8".into(),
                });
                continue;
            }
        };
        match bucket_entry.file_type() {
            Ok(file_type) if file_type.is_dir() => {}
            Ok(_) => {
                scan.diagnostics.push(CatalogDiagnostic {
                    project_bucket: Some(bucket),
                    path: bucket_path,
                    kind: CatalogDiagnosticKind::UnsafeFile,
                    message: "project bucket is not a directory".into(),
                });
                continue;
            }
            Err(error) => {
                scan.diagnostics
                    .push(io_catalog_diagnostic(Some(bucket), bucket_path, error));
                continue;
            }
        }

        let files = match fs::read_dir(&bucket_path) {
            Ok(entries) => entries,
            Err(error) => {
                scan.diagnostics
                    .push(io_catalog_diagnostic(Some(bucket), bucket_path, error));
                continue;
            }
        };
        let manager = SessionManager::with_root(&bucket_path);
        for file_result in files {
            let file_entry = match file_result {
                Ok(entry) => entry,
                Err(error) => {
                    scan.diagnostics.push(io_catalog_diagnostic(
                        Some(bucket.clone()),
                        bucket_path.clone(),
                        error,
                    ));
                    continue;
                }
            };
            let path = file_entry.path();
            let name = match file_entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => {
                    scan.diagnostics.push(CatalogDiagnostic {
                        project_bucket: Some(bucket.clone()),
                        path,
                        kind: CatalogDiagnosticKind::InvalidId,
                        message: "session filename is not valid UTF-8".into(),
                    });
                    continue;
                }
            };
            let direct_sidecar_id = name
                .strip_suffix(".snapshot")
                .or_else(|| name.strip_suffix(".jsonl"))
                .or_else(|| name.strip_suffix(".rewind.txn.json"))
                .or_else(|| name.strip_suffix(".rewind.json"));
            if let Some(id) = direct_sidecar_id {
                match file_entry.file_type() {
                    Ok(file_type) if file_type.is_file() => {}
                    Ok(_) => {
                        scan.diagnostics.push(CatalogDiagnostic {
                            project_bucket: Some(bucket.clone()),
                            path,
                            kind: CatalogDiagnosticKind::UnsafeFile,
                            message: "native session sidecar is not a regular file".into(),
                        });
                        continue;
                    }
                    Err(error) => {
                        scan.diagnostics.push(io_catalog_diagnostic(
                            Some(bucket.clone()),
                            path,
                            error,
                        ));
                        continue;
                    }
                }
                if let Err(error) = validate_session_id(id) {
                    scan.diagnostics.push(catalog_diagnostic_from_error(
                        Some(bucket.clone()),
                        path,
                        error,
                    ));
                    continue;
                }
                native_sidecars
                    .entry((bucket.clone(), id.to_string()))
                    .or_insert(path);
                continue;
            }
            let source = if let Some(id) = name.strip_suffix(".meta") {
                Some((id, false))
            } else if let Some(id) = name.strip_suffix(".json") {
                if let Some(presentation_id) = name.strip_suffix(".ui.json") {
                    let has_native_companion =
                        ["meta", "snapshot", "jsonl"].iter().any(|extension| {
                            bucket_path
                                .join(format!("{presentation_id}.{extension}"))
                                .symlink_metadata()
                                .is_ok()
                        });
                    if has_native_companion {
                        None
                    } else {
                        Some((id, true))
                    }
                } else {
                    Some((id, true))
                }
            } else {
                None
            };
            let Some((id, legacy)) = source else {
                continue;
            };
            match file_entry.file_type() {
                Ok(file_type) if file_type.is_file() => {}
                Ok(_) => {
                    scan.diagnostics.push(CatalogDiagnostic {
                        project_bucket: Some(bucket.clone()),
                        path,
                        kind: CatalogDiagnosticKind::UnsafeFile,
                        message: "catalog source is not a regular file".into(),
                    });
                    continue;
                }
                Err(error) => {
                    scan.diagnostics
                        .push(io_catalog_diagnostic(Some(bucket.clone()), path, error));
                    continue;
                }
            }
            if let Err(error) = validate_session_id(id) {
                scan.diagnostics.push(catalog_diagnostic_from_error(
                    Some(bucket.clone()),
                    path,
                    error,
                ));
                continue;
            }

            let key = (bucket.clone(), id.to_string());
            if legacy {
                match read_legacy_catalog_meta(&path, id) {
                    Ok(meta) => sessions.entry(key).or_default().legacy = Some(meta),
                    Err(error) => scan.diagnostics.push(catalog_diagnostic_from_error(
                        Some(bucket.clone()),
                        path,
                        error,
                    )),
                }
            } else {
                native_meta_ids.insert(key.clone());
                match manager.read_meta(id) {
                    Ok(meta) => sessions.entry(key).or_default().native = Some(meta),
                    Err(error) => scan.diagnostics.push(catalog_diagnostic_from_error(
                        Some(bucket.clone()),
                        path,
                        error,
                    )),
                }
            }
        }
    }

    for (key, path) in native_sidecars {
        if !native_meta_ids.contains(&key) {
            scan.diagnostics.push(CatalogDiagnostic {
                project_bucket: Some(key.0),
                path,
                kind: CatalogDiagnosticKind::Corrupt,
                message: format!("native session {:?} has sidecars but no metadata", key.1),
            });
        }
    }

    scan.entries = sessions
        .into_iter()
        .filter_map(|((project_bucket, id), sources)| catalog_entry(project_bucket, id, sources))
        .collect();
    scan.entries.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.project_bucket.cmp(&b.project_bucket))
    });
    scan.diagnostics.sort_by(|a, b| a.path.cmp(&b.path));
    scan
}

fn valid_project_bucket(bucket: &str) -> bool {
    bucket.len() == 16 && bucket.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn migrate_sessions_from(legacy_root: &Path, target_root: &Path) -> SessionResult<usize> {
    if !legacy_root.exists() {
        return Ok(0);
    }
    if target_root.exists() {
        let mut entries = fs::read_dir(target_root).map_err(|error| io_at(target_root, error))?;
        if entries.next().is_some() {
            return Ok(0);
        }
    }

    let mut copies = Vec::new();
    for bucket_entry in fs::read_dir(legacy_root).map_err(|error| io_at(legacy_root, error))? {
        let bucket_entry = bucket_entry.map_err(|error| io_at(legacy_root, error))?;
        let bucket_path = bucket_entry.path();
        let file_type = bucket_entry
            .file_type()
            .map_err(|error| io_at(&bucket_path, error))?;
        if !file_type.is_dir() {
            continue;
        }
        let bucket = bucket_entry.file_name().to_string_lossy().into_owned();
        if !valid_project_bucket(&bucket) {
            return Err(SessionStoreError::UnsafeFile {
                path: bucket_path,
                reason: "legacy session bucket is not a 16-character hex id",
            });
        }
        for file_entry in fs::read_dir(&bucket_path).map_err(|error| io_at(&bucket_path, error))? {
            let file_entry = file_entry.map_err(|error| io_at(&bucket_path, error))?;
            let source = file_entry.path();
            if !file_entry
                .file_type()
                .map_err(|error| io_at(&source, error))?
                .is_file()
            {
                return Err(SessionStoreError::UnsafeFile {
                    path: source,
                    reason: "legacy session artifact is not a regular file",
                });
            }
            copies.push((
                source,
                target_root.join(&bucket).join(file_entry.file_name()),
            ));
        }
    }

    for (source, target) in &copies {
        let parent = target.parent().expect("session target always has a bucket");
        fs::create_dir_all(parent).map_err(|error| io_at(parent, error))?;
        let mut input = File::open(source).map_err(|error| io_at(source, error))?;
        let mut output = OpenOptions::new();
        output.create_new(true).write(true);
        set_private_create_mode(&mut output);
        let mut output = output.open(target).map_err(|error| io_at(target, error))?;
        ensure_private_file_permissions(target, &output)?;
        io::copy(&mut input, &mut output).map_err(|error| io_at(target, error))?;
        output.sync_all().map_err(|error| io_at(target, error))?;
    }
    Ok(copies.len())
}

fn read_legacy_catalog_meta(path: &Path, expected_id: &str) -> SessionResult<LegacyCatalogMeta> {
    let bytes = read_regular_file_bounded(path, "legacy session", MAX_LEGACY_SESSION_BYTES)?;
    let meta: LegacyCatalogMeta = deserialize(&bytes, "legacy session")?;
    validate_session_id(&meta.id)?;
    validate_string("legacy session name", &meta.name, MAX_STORED_STRING_BYTES)?;
    validate_string(
        "legacy working directory",
        &meta.working_dir.to_string_lossy(),
        MAX_STORED_STRING_BYTES,
    )?;
    if meta.id != expected_id {
        return Err(SessionStoreError::Corrupt {
            kind: "legacy session",
            message: format!(
                "file id {expected_id:?} does not match stored id {:?}",
                meta.id
            ),
        });
    }
    checked_legacy_millis(meta.created_at)?;
    checked_legacy_millis(meta.updated_at)?;
    Ok(meta)
}

fn checked_legacy_millis(seconds: u64) -> SessionResult<i64> {
    seconds
        .checked_mul(1_000)
        .and_then(|millis| i64::try_from(millis).ok())
        .ok_or_else(|| SessionStoreError::Corrupt {
            kind: "legacy session",
            message: format!("timestamp {seconds} seconds does not fit epoch milliseconds"),
        })
}

fn catalog_entry(
    project_bucket: String,
    id: String,
    sources: CatalogAggregate,
) -> Option<CatalogEntry> {
    match (sources.native, sources.legacy) {
        (Some(native), legacy) => Some(CatalogEntry {
            id,
            fork_root_id: native.fork_info.as_ref().map(|fork| fork.root_id.clone()),
            name: native.name,
            project_bucket,
            working_dir: PathBuf::from(native.working_dir),
            created_at_ms: native.created_at,
            updated_at_ms: native.updated_at,
            message_count: native.message_count as usize,
            turn_count: native.turn_count as usize,
            presence: if legacy.is_some() {
                CatalogPresence::Both
            } else {
                CatalogPresence::NativeOnly
            },
        }),
        (None, Some(legacy)) => Some(CatalogEntry {
            id,
            name: legacy.name,
            fork_root_id: None,
            project_bucket,
            working_dir: legacy.working_dir,
            created_at_ms: checked_legacy_millis(legacy.created_at).ok()?,
            updated_at_ms: checked_legacy_millis(legacy.updated_at).ok()?,
            message_count: legacy.messages.len(),
            turn_count: legacy.turn_stats.len(),
            presence: CatalogPresence::LegacyOnly,
        }),
        (None, None) => None,
    }
}

fn unique_catalog_match(
    query: &str,
    matches: Vec<&CatalogEntry>,
) -> SessionResult<Option<CatalogEntry>> {
    match matches.as_slice() {
        [] => Ok(None),
        [entry] => Ok(Some((*entry).clone())),
        _ => Err(SessionStoreError::AmbiguousId {
            query: query.to_string(),
            matches: matches
                .into_iter()
                .map(|entry| CatalogLocation {
                    id: entry.id.clone(),
                    project_bucket: entry.project_bucket.clone(),
                })
                .collect(),
        }),
    }
}

fn catalog_diagnostic_from_error(
    project_bucket: Option<String>,
    path: PathBuf,
    error: SessionStoreError,
) -> CatalogDiagnostic {
    let kind = match &error {
        SessionStoreError::InvalidId { .. } => CatalogDiagnosticKind::InvalidId,
        SessionStoreError::TooLarge { .. } => CatalogDiagnosticKind::TooLarge,
        SessionStoreError::FutureSchema { .. } => CatalogDiagnosticKind::FutureSchema,
        SessionStoreError::Corrupt { .. } => CatalogDiagnosticKind::Corrupt,
        SessionStoreError::UnsafeFile { .. } => CatalogDiagnosticKind::UnsafeFile,
        SessionStoreError::NotFound { .. }
        | SessionStoreError::SessionInUse { .. }
        | SessionStoreError::AmbiguousId { .. }
        | SessionStoreError::LeaseMismatch { .. }
        | SessionStoreError::OwnershipConflict { .. }
        | SessionStoreError::Io { .. }
        | SessionStoreError::UncertainCommit { .. } => CatalogDiagnosticKind::Io,
    };
    CatalogDiagnostic {
        project_bucket,
        path,
        kind,
        message: error.to_string(),
    }
}

fn io_catalog_diagnostic(
    project_bucket: Option<String>,
    path: PathBuf,
    error: io::Error,
) -> CatalogDiagnostic {
    CatalogDiagnostic {
        project_bucket,
        path,
        kind: CatalogDiagnosticKind::Io,
        message: error.to_string(),
    }
}

fn validate_delete_target(path: &Path) -> SessionResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(SessionStoreError::UnsafeFile {
            path: path.to_path_buf(),
            reason: "delete target is not a regular file",
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_at(path, error)),
    }
}

fn validate_session_query(query: &str) -> SessionResult<()> {
    if query.is_empty() {
        return Err(invalid_id(query, "query must not be empty"));
    }
    if query.len() > MAX_SESSION_ID_BYTES {
        return Err(invalid_id(query, "query exceeds the maximum byte length"));
    }
    if matches!(query, "." | "..")
        || query.starts_with('/')
        || query.starts_with('\\')
        || query.contains('/')
        || query.contains('\\')
        || query
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err(invalid_id(
            query,
            "query contains a path separator or filesystem-reserved character",
        ));
    }
    Ok(())
}

fn validate_session_id(id: &str) -> SessionResult<()> {
    if id.is_empty() {
        return Err(invalid_id(id, "must not be empty"));
    }
    if id.len() > MAX_SESSION_ID_BYTES {
        return Err(invalid_id(id, "exceeds the maximum byte length"));
    }
    if matches!(id, "." | "..") {
        return Err(invalid_id(id, "dot path components are not allowed"));
    }
    if id.starts_with('/') || id.starts_with('\\') || id.contains('/') || id.contains('\\') {
        return Err(invalid_id(
            id,
            "path separators and absolute paths are not allowed",
        ));
    }
    if id
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err(invalid_id(
            id,
            "contains a control or filesystem-reserved character",
        ));
    }
    if id.ends_with(['.', ' ']) {
        return Err(invalid_id(id, "must not end with a dot or space"));
    }
    let stem = id.split('.').next().unwrap_or(id).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .is_some_and(is_windows_device_number)
        || stem
            .strip_prefix("LPT")
            .is_some_and(is_windows_device_number);
    if reserved {
        return Err(invalid_id(id, "is a reserved Windows device name"));
    }
    Ok(())
}

fn invalid_id(id: &str, reason: &'static str) -> SessionStoreError {
    let mut shown: String = id.chars().take(MAX_SESSION_ID_BYTES).collect();
    if shown.len() < id.len() {
        shown.push('…');
    }
    SessionStoreError::InvalidId { id: shown, reason }
}

fn is_windows_device_number(suffix: &str) -> bool {
    matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
}

fn validate_meta(meta: &SessionMeta) -> SessionResult<()> {
    validate_session_id(&meta.id)?;
    validate_string("session name", &meta.name, MAX_STORED_STRING_BYTES)?;
    validate_string(
        "working directory",
        &meta.working_dir,
        MAX_STORED_STRING_BYTES,
    )?;
    if let Some(import) = &meta.import_info {
        if meta.owner != StorageOwner::Native {
            return Err(SessionStoreError::Corrupt {
                kind: "session meta",
                message: "import_info requires owner=native".into(),
            });
        }
        validate_string(
            "legacy schema",
            &import.legacy_schema,
            MAX_STORED_STRING_BYTES,
        )?;
        if import.importer_version == 0 {
            return Err(SessionStoreError::Corrupt {
                kind: "session meta",
                message: "importer_version must be non-zero".into(),
            });
        }
        if import.source_sha256.len() != 64
            || !import
                .source_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(SessionStoreError::Corrupt {
                kind: "session meta",
                message: "source_sha256 must be 64 hexadecimal characters".into(),
            });
        }
    }
    if let Some(fork) = &meta.fork_info {
        if meta.owner != StorageOwner::Native {
            return Err(SessionStoreError::Corrupt {
                kind: "session meta",
                message: "fork_info requires owner=native".into(),
            });
        }
        validate_session_id(&fork.root_id)?;
        validate_session_id(&fork.parent_id)?;
        if fork.root_id == meta.id || fork.parent_id == meta.id {
            return Err(SessionStoreError::Corrupt {
                kind: "session meta",
                message: "fork lineage cannot reference the fork itself".into(),
            });
        }
        if fork.forked_at_ms < 0 {
            return Err(SessionStoreError::Corrupt {
                kind: "session meta",
                message: "forked_at_ms must be non-negative".into(),
            });
        }
    }
    if meta.turn_stats.len() > MAX_META_TURN_STATS {
        return Err(SessionStoreError::TooLarge {
            kind: "meta turn stats",
            limit: MAX_META_TURN_STATS,
            actual: meta.turn_stats.len(),
        });
    }
    Ok(())
}

fn ensure_meta_id(expected: &str, meta: &SessionMeta) -> SessionResult<()> {
    if meta.id == expected {
        Ok(())
    } else {
        Err(SessionStoreError::Corrupt {
            kind: "session meta",
            message: format!(
                "mutation changed session id from {expected:?} to {:?}",
                meta.id
            ),
        })
    }
}

fn validate_snapshot(snapshot: &SessionSnapshot) -> SessionResult<()> {
    if snapshot.version > SNAPSHOT_VERSION {
        return Err(SessionStoreError::FutureSchema {
            kind: "snapshot",
            found: snapshot.version,
            supported: SNAPSHOT_VERSION,
        });
    }
    if snapshot.messages.len() > MAX_SNAPSHOT_MESSAGES {
        return Err(SessionStoreError::TooLarge {
            kind: "snapshot messages",
            limit: MAX_SNAPSHOT_MESSAGES,
            actual: snapshot.messages.len(),
        });
    }
    for message in &snapshot.messages {
        validate_string("message text", &message.text, MAX_STORED_STRING_BYTES)?;
        if let Some(id) = &message.tool_call_id {
            validate_string("tool call id", id, MAX_STORED_STRING_BYTES)?;
        }
        if let Some(reasoning) = &message.reasoning {
            validate_string("message reasoning", reasoning, MAX_STORED_STRING_BYTES)?;
        }
        for call in &message.tool_calls {
            validate_string("tool call id", &call.id, MAX_STORED_STRING_BYTES)?;
            validate_string("tool name", &call.name, MAX_STORED_STRING_BYTES)?;
            validate_string("tool arguments", &call.arguments, MAX_STORED_STRING_BYTES)?;
        }
        for image in &message.images {
            validate_string(
                "image media type",
                &image.media_type,
                MAX_STORED_STRING_BYTES,
            )?;
            validate_string("image base64", &image.data, MAX_IMAGE_BASE64_BYTES)?;
        }
        for block in &message.reasoning_blocks {
            validate_string("reasoning block text", &block.text, MAX_STORED_STRING_BYTES)?;
            if let Some(opaque) = &block.opaque {
                validate_string("reasoning opaque payload", opaque, MAX_STORED_STRING_BYTES)?;
            }
            if let Some(provider) = &block.provider {
                validate_string("reasoning provider", provider, MAX_STORED_STRING_BYTES)?;
            }
        }
    }
    Ok(())
}

fn validate_string(kind: &'static str, value: &str, limit: usize) -> SessionResult<()> {
    if value.len() > limit {
        return Err(SessionStoreError::TooLarge {
            kind,
            limit,
            actual: value.len(),
        });
    }
    Ok(())
}

fn serialize_bounded<T: Serialize>(
    value: &T,
    kind: &'static str,
    limit: usize,
) -> SessionResult<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|e| SessionStoreError::Corrupt {
        kind,
        message: e.to_string(),
    })?;
    ensure_size(kind, bytes.len(), limit)?;
    Ok(bytes)
}

fn serialize_pretty_bounded<T: Serialize>(
    value: &T,
    kind: &'static str,
    limit: usize,
) -> SessionResult<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| SessionStoreError::Corrupt {
        kind,
        message: e.to_string(),
    })?;
    ensure_size(kind, bytes.len(), limit)?;
    Ok(bytes)
}

fn deserialize<T: for<'de> Deserialize<'de>>(bytes: &[u8], kind: &'static str) -> SessionResult<T> {
    serde_json::from_slice(bytes).map_err(|e| SessionStoreError::Corrupt {
        kind,
        message: e.to_string(),
    })
}

fn ensure_size(kind: &'static str, actual: usize, limit: usize) -> SessionResult<()> {
    if actual > limit {
        Err(SessionStoreError::TooLarge {
            kind,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

pub(crate) fn regular_file_len(path: &Path) -> SessionResult<usize> {
    let metadata = fs::symlink_metadata(path).map_err(|e| io_at(path, e))?;
    if !metadata.file_type().is_file() {
        return Err(SessionStoreError::UnsafeFile {
            path: path.to_path_buf(),
            reason: "expected a regular file; symlinks and special files are rejected",
        });
    }
    usize::try_from(metadata.len()).map_err(|_| SessionStoreError::TooLarge {
        kind: "session file",
        limit: usize::MAX,
        actual: usize::MAX,
    })
}

fn reject_existing_non_regular(path: &Path) -> SessionResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(SessionStoreError::UnsafeFile {
            path: path.to_path_buf(),
            reason: "expected a regular file; symlinks and special files are rejected",
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_at(path, error)),
    }
}

fn open_read_file(path: &Path) -> SessionResult<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    let file = options.open(path).map_err(|e| io_at(path, e))?;
    ensure_opened_regular(path, &file)?;
    Ok(file)
}

fn open_append_file(path: &Path) -> SessionResult<File> {
    let mut options = OpenOptions::new();
    // Windows: `append(true)` alone grants only FILE_APPEND_DATA, which is not
    // enough for LockFileEx (requires GENERIC_READ or GENERIC_WRITE) — the
    // transcript append below would fail with ERROR_ACCESS_DENIED (os error 5).
    // Note: `write(true)` is IGNORED when `append(true)` is set (Rust std), so
    // GENERIC_READ via `read(true)` is what satisfies LockFileEx here. Append
    // semantics are unchanged.
    options.create(true).append(true).read(true);
    set_private_create_mode(&mut options);
    no_follow(&mut options);
    let file = options.open(path).map_err(|e| io_at(path, e))?;
    ensure_opened_regular(path, &file)?;
    ensure_private_file_permissions(path, &file)?;
    Ok(file)
}

const TRANSIENT_FILE_ACCESS_RETRY_DELAYS_MS: [u64; 3] = [10, 30, 60];
const META_LOCK_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const META_LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

fn is_file_lock_contention(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        // Windows reports sharing/lock violations as raw OS errors instead of
        // consistently mapping them to WouldBlock (ERROR_SHARING_VIOLATION=32,
        // ERROR_LOCK_VIOLATION=33).
        || cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))
}

fn acquire_file_lock_until(file: &File, timeout: std::time::Duration) -> io::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match fs2::FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error) if is_file_lock_contention(&error) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(error);
                }
                std::thread::sleep(
                    META_LOCK_RETRY_DELAY.min(deadline.saturating_duration_since(now)),
                );
            }
            Err(error) => return Err(error),
        }
    }
}

fn retry_transient_file_access<T>(
    mut operation: impl FnMut() -> SessionResult<T>,
) -> SessionResult<T> {
    for delay_ms in TRANSIENT_FILE_ACCESS_RETRY_DELAYS_MS {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            Err(error) => return Err(error),
        }
    }
    operation()
}

fn open_lock_file(path: &Path) -> SessionResult<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    set_private_create_mode(&mut options);
    no_follow(&mut options);
    let file = options.open(path).map_err(|e| io_at(path, e))?;
    ensure_opened_regular(path, &file)?;
    ensure_private_file_permissions(path, &file)?;
    Ok(file)
}

fn open_session_lock_file(id: &str, path: &Path) -> SessionResult<File> {
    open_lock_file(path).map_err(|error| match error {
        SessionStoreError::Io { source, .. } if is_file_lock_contention(&source) => {
            SessionStoreError::SessionInUse {
                id: id.to_string(),
                path: path.to_path_buf(),
            }
        }
        error => error,
    })
}

fn set_private_create_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = options;
}

fn ensure_private_file_permissions(path: &Path, file: &File) -> SessionResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = file
            .metadata()
            .map_err(|error| io_at(path, error))?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| io_at(path, error))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = file;
    }
    Ok(())
}

fn ensure_opened_regular(path: &Path, file: &File) -> SessionResult<()> {
    let metadata = file.metadata().map_err(|e| io_at(path, e))?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(SessionStoreError::UnsafeFile {
            path: path.to_path_buf(),
            reason: "opened object is not a regular file",
        })
    }
}

fn no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open the link/reparse point itself so the
        // post-open regular-file check can reject it instead of following it.
        options.custom_flags(0x0020_0000);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = options;
}

fn read_regular_file_bounded(
    path: &Path,
    kind: &'static str,
    limit: usize,
) -> SessionResult<Vec<u8>> {
    let size = regular_file_len(path)?;
    ensure_size(kind, size, limit)?;
    let file = open_read_file(path)?;
    let take_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::with_capacity(size.min(limit));
    file.take(take_limit)
        .read_to_end(&mut bytes)
        .map_err(|e| io_at(path, e))?;
    ensure_size(kind, bytes.len(), limit)?;
    Ok(bytes)
}

pub(crate) fn for_each_jsonl_line(
    path: &Path,
    mut visit: impl FnMut(&[u8]) -> SessionResult<()>,
) -> SessionResult<(usize, usize)> {
    let size = regular_file_len(path)?;
    ensure_size("transcript", size, MAX_JSONL_BYTES)?;
    let file = open_read_file(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut total = 0usize;
    let mut lines = 0usize;
    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take((MAX_JSONL_LINE_BYTES as u64).saturating_add(1))
            .read_until(b'\n', &mut line)
            .map_err(|e| io_at(path, e))?;
        if read == 0 {
            break;
        }
        ensure_size("transcript line", line.len(), MAX_JSONL_LINE_BYTES)?;
        total = total
            .checked_add(line.len())
            .ok_or(SessionStoreError::TooLarge {
                kind: "transcript",
                limit: MAX_JSONL_BYTES,
                actual: usize::MAX,
            })?;
        ensure_size("transcript", total, MAX_JSONL_BYTES)?;
        lines += 1;
        if lines > MAX_JSONL_LINES {
            return Err(SessionStoreError::TooLarge {
                kind: "transcript lines",
                limit: MAX_JSONL_LINES,
                actual: lines,
            });
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        visit(&line)?;
    }
    Ok((total, lines))
}

fn io_at(path: &Path, source: io::Error) -> SessionStoreError {
    if source.kind() == io::ErrorKind::NotFound {
        SessionStoreError::NotFound {
            path: path.to_path_buf(),
        }
    } else {
        SessionStoreError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Write `bytes` to `path` atomically: write a sibling `*.tmp` then `rename` over the
/// target, then sync the parent directory on Unix so the new directory entry survives
/// power loss. A crash mid-write never leaves a half-written (corrupt) session file.
/// The tmp's extension (`…tmp`) is ignored by [`SessionManager::list`]'s `*.meta`
/// filter, so a leftover tmp from a crash never appears as a session.
fn atomic_write(path: &Path, bytes: &[u8]) -> SessionResult<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| io_at(parent, e))?;
    reject_existing_non_regular(path)?;
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session");
    let tmp = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        set_private_create_mode(&mut options);
        let mut file = options.open(&tmp).map_err(|e| io_at(&tmp, e))?;
        ensure_private_file_permissions(&tmp, &file)?;
        file.write_all(bytes).map_err(|e| io_at(&tmp, e))?;
        file.sync_all().map_err(|e| io_at(&tmp, e))?;
        fs::rename(&tmp, path).map_err(|e| io_at(path, e))?;
        #[cfg(unix)]
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| io_at(parent, e))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::presentation::{
        DisplayAnchor, PresentationEntry, PresentationFile, PresentationRole,
        MAX_PRESENTATION_BYTES, PRESENTATION_VERSION,
    };
    use super::*;
    use atomcode_kernel::message::Message;
    use std::collections::BTreeSet;

    fn snap(texts: &[&str]) -> SessionSnapshot {
        SessionSnapshot::new(texts.iter().map(|t| Message::user(*t)).collect())
    }

    fn native_artifact_bytes(mgr: &SessionManager, id: &str) -> [Vec<u8>; 3] {
        [
            std::fs::read(mgr.snapshot_path(id).unwrap()).unwrap(),
            std::fs::read(mgr.presentation_path(id).unwrap()).unwrap(),
            std::fs::read(mgr.meta_path(id).unwrap()).unwrap(),
        ]
    }

    #[test]
    fn fallback_name_uses_text_after_image_marker_in_first_user_message() {
        let mut meta = SessionMeta::new("image-session", "/project", 1);

        meta.auto_name_from_messages(&[Message::user("[Image #1]识别图片内容")]);

        assert_eq!(meta.name, "识别图片内容");
    }

    #[test]
    fn fallback_name_skips_injected_system_reminder() {
        let mut meta = SessionMeta::new("reminder-session", "/project", 1);

        meta.auto_name_from_messages(&[
            Message::user(crate::reminder::system_reminder(
                "我就在任务1上！继续任务2！",
            )),
            Message::user("修复登录错误"),
        ]);

        assert_eq!(meta.name, "修复登录错误");
    }

    #[test]
    fn fallback_name_preserves_real_bracketed_first_user_message() {
        let mut meta = SessionMeta::new("bracket-session", "/project", 1);

        meta.auto_name_from_messages(&[
            Message::user("[workspace] 修复登录"),
            Message::user("好的"),
        ]);

        assert_eq!(meta.name, "[workspace] 修复登录");
    }

    #[test]
    fn fallback_name_does_not_replace_meaningful_session_prefixed_name() {
        let mut meta = SessionMeta::new("id", "/project", 1);
        meta.name = "session-notes".into();

        meta.auto_name_from_messages(&[Message::user("不应覆盖")]);

        assert_eq!(meta.name, "session-notes");
    }

    /// project_hash must match production's `hash_path` BYTE-FOR-BYTE: hash the
    /// normalized path as a `PathBuf` via the std `DefaultHasher`, `{:016x}`. This is
    /// the regression guard that keeps the new stack on production's session bucket
    /// (a `&str` hash, or a different formatter, would orphan every legacy session).
    #[test]
    fn project_hash_matches_production_hash_path_scheme() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let p = Path::new("/Users/theo/Documents/workspace/atomcode");
        let mut expected = DefaultHasher::new();
        PathBuf::from(p.to_string_lossy().to_string()).hash(&mut expected);
        assert_eq!(
            SessionManager::project_hash(p),
            format!("{:016x}", expected.finish())
        );
    }

    #[test]
    fn project_hash_is_stable_and_normalizes_trailing_slash() {
        let a = SessionManager::project_hash(Path::new("/work/proj"));
        let b = SessionManager::project_hash(Path::new("/work/proj/"));
        assert_eq!(a, b, "a trailing slash must not change the bucket");
        assert_eq!(a.len(), 16, "16 hex chars");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn legacy_root_migration_copies_regular_bucket_files_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        let target = dir.path().join("target");
        let bucket = "0123456789abcdef";
        std::fs::create_dir_all(legacy.join(bucket)).unwrap();
        std::fs::write(legacy.join(bucket).join("session.json"), b"legacy").unwrap();

        assert_eq!(migrate_sessions_from(&legacy, &target).unwrap(), 1);
        assert_eq!(
            std::fs::read(target.join(bucket).join("session.json")).unwrap(),
            b"legacy"
        );

        std::fs::write(legacy.join(bucket).join("session.json"), b"changed").unwrap();
        assert_eq!(migrate_sessions_from(&legacy, &target).unwrap(), 0);
        assert_eq!(
            std::fs::read(target.join(bucket).join("session.json")).unwrap(),
            b"legacy",
            "an initialized canonical root must never be overwritten"
        );
    }

    #[test]
    fn legacy_root_migration_rejects_untrusted_bucket_names() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        let target = dir.path().join("target");
        std::fs::create_dir_all(legacy.join("not-a-bucket")).unwrap();
        std::fs::write(legacy.join("not-a-bucket").join("session.json"), b"legacy").unwrap();

        assert!(matches!(
            migrate_sessions_from(&legacy, &target),
            Err(SessionStoreError::UnsafeFile { .. })
        ));
        assert!(!target.join("not-a-bucket").exists());
    }

    #[test]
    fn snapshot_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let s = snap(&["hello", "world"]);
        mgr.save_snapshot("s1", &s).unwrap();
        let loaded = mgr.load_snapshot("s1").unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].text, "hello");
    }

    /// Windows 回归测试：transcript 追加必须真实完成 open → lock → append → unlock。
    /// 曾因 `open_append_file` 仅 `append(true)`（Windows 上只有 FILE_APPEND_DATA），
    /// `LockFileEx` 要求 GENERIC_READ/GENERIC_WRITE 而必然失败，
    /// 报 ERROR_ACCESS_DENIED (os error 5)。此问题仅存在于 Windows。
    #[test]
    #[cfg(windows)]
    fn append_jsonl_line_lock_roundtrip_windows() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let id = "s1";
        let mut payload = br#"{"v":1,"msg":"hello"}"#.to_vec();
        payload.push(b'\n');

        // 真实代码路径: open_append_file → lock_exclusive → write_all（unlock 随句柄关闭）
        mgr.append_jsonl_line(id, &payload).unwrap_or_else(|e| {
            panic!("transcript append must not fail on Windows: {e:?}");
        });

        // 内容确实被追加写入
        let written = std::fs::read(mgr.jsonl_path(id).unwrap()).unwrap();
        assert_eq!(written, payload);
    }

    #[test]
    fn transcript_timestamps_load_without_message_bodies_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        assert!(mgr
            .load_transcript_timestamps("missing")
            .unwrap()
            .is_empty());

        let line = serde_json::json!({
            "v": 1,
            "started_at": 1_700_000_000_000_i64,
            "ts": 1_700_000_000_123_i64,
            "iso": "2023-11-14T22:13:20.123Z",
            "session_id": "s1",
            "turn_id": 7,
            "undone": false,
            "user": "question",
            "assistant": "answer",
            "tools": [],
            "usage": { "prompt": 1, "completion": 2, "cached": 0 }
        });
        let mut bytes = serde_json::to_vec(&line).unwrap();
        bytes.push(b'\n');
        mgr.append_jsonl_line("s1", &bytes).unwrap();

        let legacy_line = serde_json::json!({
            "v": 1,
            "ts": 1_700_000_001_123_i64,
            "iso": "2023-11-14T22:13:21.123Z",
            "session_id": "s1",
            "turn_id": 8,
            "undone": false,
            "user": "legacy question",
            "assistant": "legacy answer",
            "tools": [],
            "usage": { "prompt": 1, "completion": 2, "cached": 0 }
        });
        let mut legacy_bytes = serde_json::to_vec(&legacy_line).unwrap();
        legacy_bytes.push(b'\n');
        mgr.append_jsonl_line("s1", &legacy_bytes).unwrap();

        let timestamps = mgr.load_transcript_timestamps("s1").unwrap();
        assert_eq!(timestamps.len(), 2);
        assert_eq!(timestamps[&7].started_at, Some(1_700_000_000_000));
        assert_eq!(timestamps[&7].completed_at, 1_700_000_000_123);
        assert_eq!(timestamps[&8].started_at, None);
        assert_eq!(timestamps[&8].completed_at, 1_700_000_001_123);
    }

    fn presentation_entry(anchor: DisplayAnchor, text: &str) -> PresentationEntry {
        PresentationEntry {
            anchor,
            role: PresentationRole::Assistant,
            text: text.into(),
        }
    }

    #[test]
    fn presentation_round_trips_without_changing_runtime_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let snapshot = snap(&["provider context"]);
        mgr.save_snapshot("s1", &snapshot).unwrap();
        let snapshot_bytes = std::fs::read(mgr.snapshot_path("s1").unwrap()).unwrap();
        let presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![presentation_entry(
                DisplayAnchor::AfterTurn { turn_id: 1 },
                "display only",
            )],
        };

        mgr.write_presentation("s1", &presentation).unwrap();

        assert_eq!(mgr.read_presentation("s1").unwrap(), presentation);
        assert_eq!(mgr.load_snapshot("s1").unwrap(), snapshot);
        assert_eq!(
            std::fs::read(mgr.snapshot_path("s1").unwrap()).unwrap(),
            snapshot_bytes,
            "presentation writes must not mutate provider/runtime context"
        );
        assert!(mgr.presentation_path("s1").unwrap().ends_with("s1.ui.json"));
    }

    #[test]
    fn presentation_append_and_prune_use_stable_turn_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.append_presentation("s1", presentation_entry(DisplayAnchor::AtStart, "start"))
            .unwrap();
        mgr.append_presentation(
            "s1",
            presentation_entry(DisplayAnchor::AfterTurn { turn_id: 1 }, "gone"),
        )
        .unwrap();
        mgr.append_presentation(
            "s1",
            presentation_entry(DisplayAnchor::AfterTurn { turn_id: 2 }, "keep"),
        )
        .unwrap();

        assert_eq!(
            mgr.prune_presentation("s1", &BTreeSet::from([2])).unwrap(),
            1
        );
        let file = mgr.read_presentation("s1").unwrap();
        assert_eq!(file.entries.len(), 2);
        assert_eq!(file.entries[0].anchor, DisplayAnchor::AtStart);
        assert_eq!(
            file.entries[1].anchor,
            DisplayAnchor::AfterTurn { turn_id: 2 }
        );
    }

    #[test]
    fn presentation_reader_rejects_future_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let future = serde_json::json!({ "v": PRESENTATION_VERSION + 1, "entries": [] });
        std::fs::write(
            dir.path().join("future.ui.json"),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            mgr.read_presentation("future"),
            Err(SessionStoreError::FutureSchema {
                kind: "presentation",
                ..
            })
        ));

        let file = std::fs::File::create(dir.path().join("large.ui.json")).unwrap();
        file.set_len((MAX_PRESENTATION_BYTES + 1) as u64).unwrap();
        assert!(matches!(
            mgr.read_presentation("large"),
            Err(SessionStoreError::TooLarge {
                kind: "presentation",
                limit: MAX_PRESENTATION_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn turn_stat_pruning_uses_turn_id_and_preserves_unconverted_legacy_stats() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        let stat = |turn_id, after_message| TurnStat {
            after_message,
            position_valid: true,
            turn_id,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
            model_usage: Vec::new(),
        };
        meta.turn_stats = vec![stat(0, 1), stat(1, 2), stat(2, 3)];
        mgr.write_meta(&meta).unwrap();

        assert_eq!(mgr.prune_turn_stats("s1", &BTreeSet::from([2])).unwrap(), 1);
        let ids: Vec<_> = mgr
            .read_meta("s1")
            .unwrap()
            .turn_stats
            .into_iter()
            .map(|stat| stat.turn_id)
            .collect();
        assert_eq!(ids, vec![0, 2]);
    }

    #[test]
    fn rejects_invalid_session_ids_before_file_access() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let snapshot = snap(&["x"]);

        for id in ["", ".", "..", "../escape", "a/b", r"a\b", "/absolute"] {
            assert!(
                matches!(
                    mgr.save_snapshot(id, &snapshot),
                    Err(SessionStoreError::InvalidId { .. })
                ),
                "unsafe session id must be rejected: {id:?}"
            );
        }
        assert!(
            mgr.save_snapshot("legacy-session_01", &snapshot).is_ok(),
            "safe historical non-UUID ids remain supported"
        );
    }

    #[test]
    fn rejects_future_snapshot_schema_in_the_store_reader() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut snapshot = snap(&["future"]);
        snapshot.version = atomcode_kernel::message::SNAPSHOT_VERSION + 1;
        std::fs::write(
            dir.path().join("future.snapshot"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();

        assert!(matches!(
            mgr.load_snapshot("future"),
            Err(SessionStoreError::FutureSchema {
                kind: "snapshot",
                ..
            })
        ));
    }

    #[test]
    fn size_guard_accepts_boundary_and_rejects_boundary_plus_one() {
        assert!(ensure_size("fixture", 8, 8).is_ok());
        assert!(matches!(
            ensure_size("fixture", 9, 8),
            Err(SessionStoreError::TooLarge {
                kind: "fixture",
                limit: 8,
                actual: 9
            })
        ));
    }

    #[test]
    fn rejects_oversized_snapshot_before_deserialization() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let path = dir.path().join("large.snapshot");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((MAX_SNAPSHOT_BYTES + 1) as u64).unwrap();

        assert!(matches!(
            mgr.load_snapshot("large"),
            Err(SessionStoreError::TooLarge {
                kind: "snapshot",
                limit: MAX_SNAPSHOT_BYTES,
                actual
            }) if actual == MAX_SNAPSHOT_BYTES + 1
        ));
    }

    #[test]
    fn transcript_append_rejects_an_oversized_line_without_creating_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let line = vec![b'x'; MAX_JSONL_LINE_BYTES + 1];

        assert!(matches!(
            mgr.append_jsonl_line("s1", &line),
            Err(SessionStoreError::TooLarge {
                kind: "transcript line",
                limit: MAX_JSONL_LINE_BYTES,
                ..
            })
        ));
        assert!(!mgr.jsonl_path("s1").unwrap().exists());
    }

    #[test]
    fn transient_file_access_is_retried_before_transcript_write() {
        let path = Path::new("transient.jsonl");
        let mut attempts = 0;

        let result = retry_transient_file_access(|| {
            attempts += 1;
            if attempts == 1 {
                Err(io_at(
                    path,
                    io::Error::new(io::ErrorKind::PermissionDenied, "temporarily busy"),
                ))
            } else {
                Ok("opened")
            }
        });

        assert_eq!(result.unwrap(), "opened");
        assert_eq!(attempts, 2);
    }

    #[test]
    fn permanent_file_access_error_is_not_retried() {
        let path = Path::new("broken.jsonl");
        let mut attempts = 0;

        let result = retry_transient_file_access(|| {
            attempts += 1;
            Err::<(), _>(io_at(
                path,
                io::Error::new(io::ErrorKind::InvalidData, "broken"),
            ))
        });

        assert!(matches!(result, Err(SessionStoreError::Io { .. })));
        assert_eq!(attempts, 1);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_snapshot_reads() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let target = dir.path().join("target.json");
        std::fs::write(&target, serde_json::to_vec(&snap(&["secret"])).unwrap()).unwrap();
        symlink(&target, dir.path().join("linked.snapshot")).unwrap();

        assert!(matches!(
            mgr.load_snapshot("linked"),
            Err(SessionStoreError::UnsafeFile { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_does_not_follow_predictable_tmp_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"keep-me").unwrap();
        symlink(&victim, dir.path().join("s1.snapshot.tmp")).unwrap();

        mgr.save_snapshot("s1", &snap(&["x"])).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep-me");
    }

    #[test]
    fn meta_round_trips_and_list_is_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.write_meta(&SessionMeta::new("old", "/p", 1_000))
            .unwrap();
        mgr.write_meta(&SessionMeta::new("new", "/p", 2_000))
            .unwrap();

        let read = mgr.read_meta("old").unwrap();
        assert_eq!(read.id, "old");
        assert_eq!(read.created_at, 1_000);

        let list = mgr.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "new", "newest updated_at first");
        assert_eq!(mgr.latest().unwrap().id, "new");
    }

    #[test]
    fn legacy_meta_rewrites_with_s1a_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let legacy = serde_json::json!({
            "v": 1,
            "id": "legacy",
            "name": "Legacy",
            "user_renamed": false,
            "working_dir": "/p",
            "created_at": 1,
            "updated_at": 2,
            "turn_count": 1,
            "message_count": 2,
            "turn_stats": [{
                "after_message": 2,
                "tool_call_count": 1,
                "duration_ms": 10,
                "total_tokens": 7,
                "errored": false
            }]
        });
        std::fs::write(
            mgr.meta_path("legacy").unwrap(),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let meta = mgr.read_meta("legacy").unwrap();
        mgr.write_meta(&meta).unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&std::fs::read(mgr.meta_path("legacy").unwrap()).unwrap())
                .unwrap();

        assert_eq!(rewritten["ai_named"], false);
        assert_eq!(rewritten["turn_stats"][0]["round_count"], 0);
        assert_eq!(rewritten["turn_stats"][0]["position_valid"], true);
        assert_eq!(rewritten["turn_stats"][0]["used_tokens"], 0);
        assert_eq!(rewritten["turn_stats"][0]["ctx_window"], 0);
        assert_eq!(meta.turn_stats[0].turn_id, 0);
        assert_eq!(rewritten["turn_stats"][0]["turn_id"], 0);
        assert_eq!(meta.owner, StorageOwner::Unconfirmed);
        assert!(meta.import_info.is_none());
        assert!(meta.fork_info.is_none());
        assert_eq!(rewritten["owner"], "unconfirmed");
        assert!(rewritten["import_info"].is_null());
        assert!(rewritten["fork_info"].is_null());
    }

    #[test]
    fn import_metadata_round_trips_separately_from_owner() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("legacy", "/p", 1);
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: "core-session-json".into(),
            source_sha256: "ab".repeat(32),
            importer_version: 1,
            kind: ImportKind::Full,
        });

        mgr.write_meta(&meta).unwrap();
        assert_eq!(mgr.read_meta("legacy").unwrap(), meta);
    }

    #[test]
    fn native_import_commit_validates_all_payloads_and_commits_meta_last() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let snapshot = snap(&["kept"]);
        let presentation = PresentationFile::default();
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        meta.import_info = Some(ImportInfo {
            legacy_schema: "core-session-json".into(),
            source_sha256: "ab".repeat(32),
            importer_version: 1,
            kind: ImportKind::Full,
        });
        let stale = dir.path().join(".s1.snapshot.999.1.tmp");
        std::fs::write(&stale, b"stale").unwrap();

        mgr.commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();

        assert_eq!(mgr.load_snapshot("s1").unwrap(), snapshot);
        assert_eq!(mgr.read_presentation("s1").unwrap(), presentation);
        assert_eq!(mgr.read_meta("s1").unwrap(), meta);
        assert!(!stale.exists());
    }

    #[test]
    fn missing_presentation_repair_is_explicit_and_reloads_a_strict_aggregate() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let snapshot = snap(&["kept"]);
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &lease,
            Some(&snapshot),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();

        std::fs::remove_file(mgr.presentation_path("s1").unwrap()).unwrap();
        assert!(matches!(
            mgr.repair_missing_presentation(&lease, false).unwrap(),
            NativeSessionRepairOutcome::RepairableMissingPresentation { .. }
        ));
        assert!(!mgr.presentation_path("s1").unwrap().exists());

        let repaired = mgr.repair_missing_presentation(&lease, true).unwrap();
        assert!(matches!(repaired, NativeSessionRepairOutcome::Repaired(_)));
        assert_eq!(mgr.load_native_session("s1").unwrap().snapshot, snapshot);
        assert_eq!(
            mgr.read_presentation("s1").unwrap(),
            PresentationFile::default()
        );
        assert!(matches!(
            mgr.repair_missing_presentation(&lease, true).unwrap(),
            NativeSessionRepairOutcome::Healthy(_)
        ));
    }

    #[test]
    fn missing_presentation_repair_never_overwrites_corrupt_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let snapshot = snap(&["kept"]);
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &lease,
            Some(&snapshot),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();
        let path = mgr.presentation_path("s1").unwrap();
        std::fs::write(&path, b"corrupt presentation").unwrap();

        assert!(matches!(
            mgr.repair_missing_presentation(&lease, true),
            Err(SessionStoreError::Corrupt {
                kind: "presentation",
                ..
            })
        ));
        assert_eq!(std::fs::read(path).unwrap(), b"corrupt presentation");
    }

    #[test]
    fn loaded_native_session_requires_native_owner_and_complete_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let snapshot = snap(&["kept"]);
        let presentation = PresentationFile::default();
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;

        mgr.commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        let loaded = mgr.load_native_session("s1").unwrap();
        assert_eq!(loaded.meta, meta);
        assert_eq!(loaded.snapshot, snapshot);
        assert_eq!(loaded.presentation, presentation);

        std::fs::remove_file(mgr.presentation_path("s1").unwrap()).unwrap();
        assert!(matches!(
            mgr.load_native_session("s1"),
            Err(SessionStoreError::NotFound { .. })
        ));

        let mut unconfirmed = SessionMeta::new("pending", "/p", 1);
        unconfirmed.owner = StorageOwner::Unconfirmed;
        mgr.write_meta(&unconfirmed).unwrap();
        assert!(matches!(
            mgr.load_native_session("pending"),
            Err(SessionStoreError::OwnershipConflict {
                owner: StorageOwner::Unconfirmed,
                ..
            })
        ));
    }

    #[test]
    fn fork_native_session_clones_committed_aggregate_while_source_is_leased() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let source_lease = mgr.acquire_lease("source").unwrap();
        let snapshot = snap(&["first", "second"]);
        let presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![presentation_entry(DisplayAnchor::AtStart, "first")],
        };
        let mut source_meta = SessionMeta::new("source", "/project", 10);
        source_meta.owner = StorageOwner::Native;
        source_meta.name = "working session".into();
        source_meta.user_renamed = true;
        source_meta.turn_count = 2;
        source_meta.message_count = 2;
        source_meta.import_info = Some(ImportInfo {
            legacy_schema: "legacy".into(),
            source_sha256: "ab".repeat(32),
            importer_version: 1,
            kind: ImportKind::Full,
        });
        mgr.commit_native_import(
            &source_lease,
            Some(&snapshot),
            Some(&presentation),
            &source_meta,
        )
        .unwrap();

        let (forked, fork_lease) = mgr
            .fork_native_session("source", "destination", 20)
            .unwrap();

        assert_eq!(fork_lease.id(), "destination");
        assert_eq!(forked.snapshot, snapshot);
        assert_eq!(forked.presentation, presentation);
        assert_eq!(forked.meta.id, "destination");
        assert_eq!(forked.meta.name, "working session");
        assert_eq!(forked.meta.working_dir, "/project");
        assert_eq!(forked.meta.created_at, 20);
        assert_eq!(forked.meta.updated_at, 20);
        assert_eq!(forked.meta.turn_count, 2);
        assert_eq!(forked.meta.message_count, 2);
        assert_eq!(forked.meta.import_info, None);
        assert_eq!(
            forked.meta.fork_info,
            Some(ForkInfo {
                root_id: "source".into(),
                parent_id: "source".into(),
                forked_at_ms: 20,
                base_message_count: 2,
                base_turn_count: 2,
            })
        );
        assert_eq!(mgr.load_native_session("source").unwrap().meta, source_meta);
        assert!(matches!(
            mgr.acquire_lease("source"),
            Err(SessionStoreError::SessionInUse { .. })
        ));
        assert!(matches!(
            mgr.acquire_lease("destination"),
            Err(SessionStoreError::SessionInUse { .. })
        ));
    }

    #[test]
    fn fork_native_session_does_not_publish_destination_when_source_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());

        assert!(matches!(
            mgr.fork_native_session("missing", "destination", 20),
            Err(SessionStoreError::NotFound { .. })
        ));
        assert!(!mgr.meta_path("destination").unwrap().exists());
        assert!(!mgr.snapshot_path("destination").unwrap().exists());
        assert!(!mgr.presentation_path("destination").unwrap().exists());
    }

    #[test]
    fn fork_native_session_refuses_to_replace_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let source_lease = mgr.acquire_lease("source").unwrap();
        let snapshot = snap(&["source"]);
        let presentation = PresentationFile::default();
        let mut source_meta = SessionMeta::new("source", "/project", 10);
        source_meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &source_lease,
            Some(&snapshot),
            Some(&presentation),
            &source_meta,
        )
        .unwrap();
        let destination_lease = mgr.acquire_lease("destination").unwrap();
        let destination_snapshot = snap(&["destination"]);
        let mut destination_meta = SessionMeta::new("destination", "/project", 11);
        destination_meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &destination_lease,
            Some(&destination_snapshot),
            Some(&presentation),
            &destination_meta,
        )
        .unwrap();
        drop(destination_lease);

        assert!(matches!(
            mgr.fork_native_session("source", "destination", 20),
            Err(SessionStoreError::OwnershipConflict { .. })
        ));
        assert_eq!(
            mgr.load_native_session("destination").unwrap().snapshot,
            destination_snapshot
        );
    }

    #[test]
    fn fork_native_session_reaps_only_unused_unleased_forks_in_the_same_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let source_lease = mgr.acquire_lease("source").unwrap();
        let snapshot = snap(&["source"]);
        let presentation = PresentationFile::default();
        let mut source_meta = SessionMeta::new("source", "/project", 10);
        source_meta.owner = StorageOwner::Native;
        source_meta.message_count = 1;
        mgr.commit_native_import(
            &source_lease,
            Some(&snapshot),
            Some(&presentation),
            &source_meta,
        )
        .unwrap();

        for (id, transcript) in [("unused", false), ("used", true)] {
            let lease = mgr.acquire_lease(id).unwrap();
            let mut meta = SessionMeta::new(id, "/project", 15);
            meta.owner = StorageOwner::Native;
            meta.message_count = 1;
            meta.fork_info = Some(ForkInfo {
                root_id: "source".into(),
                parent_id: "source".into(),
                forked_at_ms: 15,
                base_message_count: 1,
                base_turn_count: 0,
            });
            mgr.commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
                .unwrap();
            if transcript {
                std::fs::write(mgr.jsonl_path(id).unwrap(), b"{\"turn\":1}\n").unwrap();
            }
            drop(lease);
        }

        let (_forked, _lease) = mgr
            .fork_native_session("source", "destination", 20)
            .unwrap();

        assert!(!mgr.meta_path("unused").unwrap().exists());
        assert!(!mgr.snapshot_path("unused").unwrap().exists());
        assert!(mgr.lease_path("unused").unwrap().exists());
        assert!(mgr.meta_lock_path("unused").unwrap().exists());
        assert!(mgr.meta_path("used").unwrap().exists());
    }

    #[test]
    fn catalog_collapse_keeps_newest_fork_but_raw_catalog_keeps_every_aggregate() {
        let root = tempfile::tempdir().unwrap();
        let bucket = root.path().join("0123456789abcdef");
        let mgr = SessionManager::with_root(&bucket);
        for (id, updated_at, fork_root_id) in [
            ("root", 10, None),
            ("fork-a", 20, Some("root")),
            ("fork-b", 30, Some("root")),
        ] {
            let mut meta = SessionMeta::new(id, "/project", updated_at);
            meta.owner = StorageOwner::Native;
            meta.message_count = 1;
            meta.fork_info = fork_root_id.map(|root_id| ForkInfo {
                root_id: root_id.into(),
                parent_id: "root".into(),
                forked_at_ms: updated_at,
                base_message_count: 1,
                base_turn_count: 0,
            });
            let lease = mgr.acquire_lease(id).unwrap();
            mgr.commit_native_import(
                &lease,
                Some(&snap(&[id])),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();
        }

        let raw = SessionManager::scan_catalog(root.path());
        assert_eq!(raw.entries.len(), 3);
        let mut visible = raw.entries.clone();
        SessionManager::collapse_fork_lineages(&mut visible);
        assert_eq!(
            visible
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fork-b"]
        );
        assert!(mgr.load_native_session("fork-a").is_ok());
    }

    #[test]
    fn native_import_invalid_sidecar_does_not_publish_snapshot_or_meta() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let invalid = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![presentation_entry(
                DisplayAnchor::AfterTurn { turn_id: 0 },
                "invalid",
            )],
        };
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;

        assert!(mgr
            .commit_native_import(&lease, Some(&snap(&["new"])), Some(&invalid), &meta)
            .is_err());
        assert!(!mgr.snapshot_path("s1").unwrap().exists());
        assert!(!mgr.meta_path("s1").unwrap().exists());
    }

    #[test]
    fn native_import_never_publishes_an_incomplete_aggregate() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut legacy = SessionMeta::new("s1", "/p", 1);
        legacy.owner = StorageOwner::Legacy;
        mgr.write_meta(&legacy).unwrap();
        std::fs::write(mgr.snapshot_path("s1").unwrap(), b"not a snapshot").unwrap();
        std::fs::write(
            mgr.presentation_path("s1").unwrap(),
            serde_json::to_vec(&PresentationFile::default()).unwrap(),
        )
        .unwrap();
        let mut native = legacy.clone();
        native.owner = StorageOwner::Native;

        assert!(mgr
            .commit_native_import(&lease, None, None, &native)
            .is_err());
        assert_eq!(mgr.read_meta("s1").unwrap(), legacy);

        let fresh_lease = mgr.acquire_lease("fresh").unwrap();
        let mut fresh = SessionMeta::new("fresh", "/p", 1);
        fresh.owner = StorageOwner::Native;
        assert!(matches!(
            mgr.commit_native_import(&fresh_lease, None, None, &fresh),
            Err(SessionStoreError::Corrupt {
                kind: "session import",
                ..
            })
        ));
        assert!(!mgr.meta_path("fresh").unwrap().exists());
    }

    #[test]
    fn unconfirmed_import_requires_full_expected_state_cas() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let expected_meta = SessionMeta::new("s1", "/p", 1);
        let expected_snapshot = snap(&["native"]);
        let expected_presentation = PresentationFile::default();
        mgr.write_meta(&expected_meta).unwrap();
        mgr.save_snapshot("s1", &expected_snapshot).unwrap();
        mgr.write_presentation("s1", &expected_presentation)
            .unwrap();
        let mut native = expected_meta.clone();
        native.owner = StorageOwner::Native;

        assert!(matches!(
            mgr.commit_native_import(&lease, None, None, &native),
            Err(SessionStoreError::OwnershipConflict {
                owner: StorageOwner::Unconfirmed,
                ..
            })
        ));
        let outcome = mgr
            .commit_native_import_if_unchanged(
                &lease,
                &expected_meta,
                Some(&expected_snapshot),
                Some(&expected_presentation),
                None,
                None,
                &native,
            )
            .unwrap();

        assert_eq!(
            outcome,
            NativeImportCommitOutcome::Committed(native.clone())
        );
        assert_eq!(mgr.load_native_session("s1").unwrap().meta, native);
    }

    #[test]
    fn native_import_cas_conflict_returns_fresh_complete_state_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let expected_meta = SessionMeta::new("s1", "/p", 1);
        let expected_snapshot = snap(&["before"]);
        let expected_presentation = PresentationFile::default();
        mgr.write_meta(&expected_meta).unwrap();
        mgr.save_snapshot("s1", &expected_snapshot).unwrap();
        mgr.write_presentation("s1", &expected_presentation)
            .unwrap();
        let mut concurrent_meta = expected_meta.clone();
        concurrent_meta.name = "concurrent rename".into();
        concurrent_meta.user_renamed = true;
        mgr.write_meta(&concurrent_meta).unwrap();
        let concurrent_snapshot = snap(&["concurrent"]);
        mgr.save_snapshot("s1", &concurrent_snapshot).unwrap();
        let mut concurrent_presentation = expected_presentation.clone();
        concurrent_presentation.entries.push(presentation_entry(
            DisplayAnchor::AtStart,
            "concurrent append",
        ));
        mgr.write_presentation("s1", &concurrent_presentation)
            .unwrap();
        let before = native_artifact_bytes(&mgr, "s1");
        let mut desired_meta = expected_meta.clone();
        desired_meta.owner = StorageOwner::Native;

        let outcome = mgr
            .commit_native_import_if_unchanged(
                &lease,
                &expected_meta,
                Some(&expected_snapshot),
                Some(&expected_presentation),
                None,
                None,
                &desired_meta,
            )
            .unwrap();

        assert_eq!(
            outcome,
            NativeImportCommitOutcome::Conflict {
                meta: concurrent_meta,
                snapshot: Some(concurrent_snapshot),
                presentation: Some(concurrent_presentation),
            }
        );
        assert_eq!(native_artifact_bytes(&mgr, "s1"), before);
    }

    #[test]
    fn empty_metadata_only_recovery_cas_preserves_concurrent_native_state() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let expected_snapshot = SessionSnapshot::new(Vec::new());
        let expected_presentation = PresentationFile::default();
        let mut expected_meta = SessionMeta::new("s1", "/p", 1);
        expected_meta.owner = StorageOwner::Native;
        expected_meta.import_info = Some(ImportInfo {
            legacy_schema: "legacy".into(),
            source_sha256: "a".repeat(64),
            importer_version: 1,
            kind: ImportKind::MetadataOnly,
        });
        mgr.commit_native_import(
            &lease,
            Some(&expected_snapshot),
            Some(&expected_presentation),
            &expected_meta,
        )
        .unwrap();
        let concurrent_snapshot = snap(&["concurrent"]);
        mgr.save_snapshot("s1", &concurrent_snapshot).unwrap();
        let before = native_artifact_bytes(&mgr, "s1");
        let desired_snapshot = snap(&["legacy"]);
        let mut desired_meta = expected_meta.clone();
        desired_meta.message_count = 1;
        desired_meta.import_info.as_mut().unwrap().kind = ImportKind::Full;

        let outcome = mgr
            .recover_empty_metadata_only_import_if_unchanged(
                &lease,
                &expected_meta,
                &expected_snapshot,
                &expected_presentation,
                &desired_snapshot,
                &expected_presentation,
                &desired_meta,
            )
            .unwrap();

        assert!(matches!(
            outcome,
            NativeImportCommitOutcome::Conflict {
                snapshot: Some(snapshot),
                ..
            } if snapshot == concurrent_snapshot
        ));
        assert_eq!(native_artifact_bytes(&mgr, "s1"), before);
    }

    #[test]
    fn native_import_cas_treats_absent_sidecars_as_exact_expectations() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let expected_meta = SessionMeta::new("s1", "/p", 1);
        let expected_snapshot = snap(&["before"]);
        mgr.write_meta(&expected_meta).unwrap();
        mgr.save_snapshot("s1", &expected_snapshot).unwrap();
        let concurrent_presentation = PresentationFile::default();
        mgr.write_presentation("s1", &concurrent_presentation)
            .unwrap();
        let mut desired_meta = expected_meta.clone();
        desired_meta.owner = StorageOwner::Native;

        let outcome = mgr
            .commit_native_import_if_unchanged(
                &lease,
                &expected_meta,
                Some(&expected_snapshot),
                None,
                None,
                Some(&concurrent_presentation),
                &desired_meta,
            )
            .unwrap();

        assert_eq!(
            outcome,
            NativeImportCommitOutcome::Conflict {
                meta: expected_meta.clone(),
                snapshot: Some(expected_snapshot.clone()),
                presentation: Some(concurrent_presentation.clone()),
            }
        );
        assert_eq!(mgr.read_meta("s1").unwrap(), expected_meta);

        std::fs::remove_file(mgr.snapshot_path("s1").unwrap()).unwrap();
        let outcome = mgr
            .commit_native_import_if_unchanged(
                &lease,
                &expected_meta,
                Some(&expected_snapshot),
                Some(&concurrent_presentation),
                Some(&expected_snapshot),
                None,
                &desired_meta,
            )
            .unwrap();
        assert!(matches!(
            outcome,
            NativeImportCommitOutcome::Conflict { snapshot: None, .. }
        ));
        assert_eq!(mgr.read_meta("s1").unwrap(), expected_meta);
        assert!(!mgr.snapshot_path("s1").unwrap().exists());
    }

    #[test]
    fn native_import_cas_rolls_back_all_preimages_after_meta_replacement_error() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let expected_meta = SessionMeta::new("s1", "/p", 1);
        let expected_snapshot = snap(&["before"]);
        let expected_presentation = PresentationFile::default();
        mgr.write_meta(&expected_meta).unwrap();
        mgr.save_snapshot("s1", &expected_snapshot).unwrap();
        mgr.write_presentation("s1", &expected_presentation)
            .unwrap();
        let before = native_artifact_bytes(&mgr, "s1");
        let mut desired_meta = expected_meta.clone();
        desired_meta.owner = StorageOwner::Native;
        let desired_snapshot = snap(&["after"]);
        mgr.fail_commit_write(CommitArtifact::Meta, CommitFaultTiming::AfterReplace);

        let error = mgr
            .commit_native_import_if_unchanged(
                &lease,
                &expected_meta,
                Some(&expected_snapshot),
                Some(&expected_presentation),
                Some(&desired_snapshot),
                None,
                &desired_meta,
            )
            .unwrap_err();

        assert!(matches!(error, SessionStoreError::Io { .. }));
        assert_eq!(native_artifact_bytes(&mgr, "s1"), before);
    }

    #[test]
    fn legacy_import_rollback_restores_corrupt_sidecar_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut legacy = SessionMeta::new("s1", "/p", 1);
        legacy.owner = StorageOwner::Legacy;
        mgr.write_meta(&legacy).unwrap();
        let corrupt_snapshot = b"corrupt snapshot";
        let corrupt_presentation = b"corrupt presentation";
        std::fs::write(mgr.snapshot_path("s1").unwrap(), corrupt_snapshot).unwrap();
        std::fs::write(mgr.presentation_path("s1").unwrap(), corrupt_presentation).unwrap();
        let mut native = legacy.clone();
        native.owner = StorageOwner::Native;
        mgr.fail_commit_write(CommitArtifact::Meta, CommitFaultTiming::AfterReplace);

        let error = mgr
            .commit_native_import(
                &lease,
                Some(&snap(&["replacement"])),
                Some(&PresentationFile::default()),
                &native,
            )
            .unwrap_err();

        assert!(matches!(error, SessionStoreError::Io { .. }));
        assert_eq!(
            std::fs::read(mgr.snapshot_path("s1").unwrap()).unwrap(),
            corrupt_snapshot
        );
        assert_eq!(
            std::fs::read(mgr.presentation_path("s1").unwrap()).unwrap(),
            corrupt_presentation
        );
        assert_eq!(mgr.read_meta("s1").unwrap(), legacy);
    }

    #[test]
    fn native_import_idempotency_never_replaces_different_native_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        let original_snapshot = snap(&["original"]);
        let original_presentation = PresentationFile::default();
        mgr.commit_native_import(
            &lease,
            Some(&original_snapshot),
            Some(&original_presentation),
            &meta,
        )
        .unwrap();
        let before = native_artifact_bytes(&mgr, "s1");
        let replacement_presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![presentation_entry(DisplayAnchor::AtStart, "replacement")],
        };

        let error = mgr
            .commit_native_import(
                &lease,
                Some(&snap(&["replacement"])),
                Some(&replacement_presentation),
                &meta,
            )
            .unwrap_err();

        assert!(matches!(error, SessionStoreError::OwnershipConflict { .. }));
        assert_eq!(native_artifact_bytes(&mgr, "s1"), before);
    }

    #[test]
    fn owner_legacy_rejects_normal_native_writers_but_import_commit_can_cut_over() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut legacy = SessionMeta::new("s1", "/p", 1);
        legacy.owner = StorageOwner::Legacy;
        mgr.write_meta(&legacy).unwrap();

        assert!(matches!(
            mgr.save_snapshot("s1", &snap(&["blocked"])),
            Err(SessionStoreError::OwnershipConflict { .. })
        ));
        assert!(matches!(
            mgr.write_presentation("s1", &PresentationFile::default()),
            Err(SessionStoreError::OwnershipConflict { .. })
        ));
        assert!(matches!(
            mgr.rename("s1", "blocked"),
            Err(SessionStoreError::OwnershipConflict { .. })
        ));
        assert!(!mgr.snapshot_path("s1").unwrap().exists());
        assert_eq!(mgr.read_meta("s1").unwrap().name, legacy.name);

        let lease = mgr.acquire_lease("s1").unwrap();
        let mut native = legacy;
        native.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &lease,
            Some(&snap(&["committed"])),
            Some(&PresentationFile::default()),
            &native,
        )
        .unwrap();
        assert_eq!(mgr.read_meta("s1").unwrap().owner, StorageOwner::Native);
    }

    #[test]
    fn rename_preserves_ai_named_from_native_meta() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let native = serde_json::json!({
            "v": 1,
            "id": "named",
            "name": "AI title",
            "user_renamed": false,
            "ai_named": true,
            "owner": "native",
            "working_dir": "/p",
            "created_at": 1,
            "updated_at": 2,
            "turn_count": 1,
            "message_count": 2,
            "turn_stats": []
        });
        std::fs::write(
            mgr.meta_path("named").unwrap(),
            serde_json::to_vec_pretty(&native).unwrap(),
        )
        .unwrap();

        mgr.rename("named", "User title").unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&std::fs::read(mgr.meta_path("named").unwrap()).unwrap())
                .unwrap();

        assert_eq!(rewritten["name"], "User title");
        assert_eq!(rewritten["user_renamed"], true);
        assert_eq!(rewritten["ai_named"], true);
    }

    #[test]
    fn list_ignores_non_meta_files_incl_production_json() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.write_meta(&SessionMeta::new("s1", "/p", 1)).unwrap();
        // A production-style `<id>.json` + a snapshot/jsonl must NOT be listed.
        std::fs::write(dir.path().join("legacy.json"), b"{\"id\":\"legacy\"}").unwrap();
        mgr.save_snapshot("s1", &snap(&["x"])).unwrap();
        std::fs::write(mgr.jsonl_path("s1").unwrap(), b"{}\n").unwrap();

        let ids: Vec<_> = mgr.list().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["s1"], "only *.meta is a session; got {ids:?}");
    }

    #[test]
    fn rename_sets_name_and_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        mgr.write_meta(&meta).unwrap();
        mgr.rename("s1", "My Session").unwrap();
        let m = mgr.read_meta("s1").unwrap();
        assert_eq!(m.name, "My Session");
        assert!(m.user_renamed);
    }

    fn write_legacy_catalog_session(
        bucket: &Path,
        id: &str,
        working_dir: &str,
        updated_at_secs: u64,
    ) {
        std::fs::create_dir_all(bucket).unwrap();
        let session = serde_json::json!({
            "id": id,
            "name": format!("legacy-{id}"),
            "working_dir": working_dir,
            "created_at": 1,
            "updated_at": updated_at_secs,
            "messages": []
        });
        std::fs::write(
            bucket.join(format!("{id}.json")),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn catalog_signature_is_stable_and_changes_on_mutation() {
        let root = tempfile::tempdir().unwrap();
        let bucket = root.path().join("0123456789abcdef");
        let mgr = SessionManager::with_root(&bucket);
        mgr.write_meta(&SessionMeta::new("a", "/p", 1_000)).unwrap();

        let sig1 = catalog_signature(root.path());
        // Same on-disk state → identical fingerprint (this is what makes a hit).
        assert_eq!(sig1, catalog_signature(root.path()));

        // Adding a session changes the fingerprint.
        mgr.write_meta(&SessionMeta::new("b", "/p", 2_000)).unwrap();
        let sig2 = catalog_signature(root.path());
        assert_ne!(sig1, sig2, "adding a .meta must change the fingerprint");

        // Removing it changes the fingerprint again.
        std::fs::remove_file(bucket.join("b.meta")).unwrap();
        let sig3 = catalog_signature(root.path());
        assert_ne!(sig2, sig3, "removing a .meta must change the fingerprint");
    }

    #[test]
    fn scan_catalog_cache_hit_returns_same_and_self_invalidates() {
        let root = tempfile::tempdir().unwrap();
        let bucket = root.path().join("0123456789abcdef");
        let mgr = SessionManager::with_root(&bucket);
        clear_catalog_cache();
        mgr.write_meta(&SessionMeta::new("a", "/p", 1_000)).unwrap();

        // First scan populates the cache; the second (no change) takes the
        // cache-hit path and must return the same entries.
        let first = SessionManager::scan_catalog(root.path());
        let second = SessionManager::scan_catalog(root.path());
        assert_eq!(first.entries.len(), 1);
        assert_eq!(
            first
                .entries
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            second
                .entries
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
        );

        // Adding a session changes the fingerprint → the next scan reflects it
        // WITHOUT any explicit cache clear (the signature self-invalidates).
        mgr.write_meta(&SessionMeta::new("b", "/p", 2_000)).unwrap();
        let third = SessionManager::scan_catalog(root.path());
        assert_eq!(third.entries.len(), 2, "rescan must see the added session");
        assert!(third.entries.iter().any(|e| e.id == "b"));
    }

    #[test]
    fn catalog_scan_merges_physical_sources_and_keeps_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let bucket = root.path().join("0123456789abcdef");
        let mgr = SessionManager::with_root(&bucket);
        let mut both = SessionMeta::new("both", "/native", 2_000);
        both.name = "native-both".into();
        both.updated_at = 4_000;
        mgr.write_meta(&both).unwrap();
        write_legacy_catalog_session(&bucket, "both", "/legacy", 3);
        write_legacy_catalog_session(&bucket, "legacy", "/legacy-only", 2);
        mgr.write_meta(&SessionMeta::new("native", "/native-only", 1_000))
            .unwrap();
        std::fs::write(bucket.join("broken.meta"), b"{").unwrap();
        std::fs::write(
            bucket.join("broken-legacy.json"),
            br#"{"id":"broken-legacy","name":"broken","working_dir":"/p","created_at":1,"updated_at":1,"messages":"not-an-array"}"#,
        )
        .unwrap();
        std::fs::write(
            bucket.join("future.meta"),
            br#"{"v":999,"id":"future","name":"future","working_dir":"/p","created_at":1,"updated_at":1}"#,
        )
        .unwrap();

        let scan = SessionManager::scan_catalog(root.path());

        assert_eq!(scan.entries.len(), 3);
        let both = scan
            .entries
            .iter()
            .find(|entry| entry.id == "both")
            .unwrap();
        assert_eq!(both.presence, CatalogPresence::Both);
        assert_eq!(both.project_bucket, "0123456789abcdef");
        assert_eq!(both.working_dir, PathBuf::from("/native"));
        assert_eq!(both.updated_at_ms, 4_000);
        assert_eq!(
            scan.entries
                .iter()
                .find(|entry| entry.id == "legacy")
                .unwrap()
                .presence,
            CatalogPresence::LegacyOnly
        );
        assert_eq!(
            scan.entries
                .iter()
                .find(|entry| entry.id == "native")
                .unwrap()
                .presence,
            CatalogPresence::NativeOnly
        );
        assert_eq!(scan.diagnostics.len(), 3);
        assert!(scan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path.ends_with("broken.meta")));
        assert!(scan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == CatalogDiagnosticKind::FutureSchema));
        assert!(scan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path.ends_with("broken-legacy.json")));
        assert_eq!(scan.latest().unwrap().id, "both");
        assert_eq!(scan.search_name("LEGACY").len(), 1);
        assert_eq!(scan.search_name("LEGACY")[0].id, "legacy");
    }

    #[test]
    fn catalog_treats_rewind_json_files_as_native_sidecars_not_legacy_sessions() {
        let root = tempfile::tempdir().unwrap();
        let bucket = root.path().join("0123456789abcdef");
        let manager = SessionManager::with_root(&bucket);
        let id = "rewind-catalog";
        manager
            .write_meta(&SessionMeta::new(id, "/project", 1))
            .unwrap();
        manager
            .save_rewind_ledger(
                id,
                &super::super::rewind::RewindLedger {
                    version: super::super::rewind::LEDGER_VERSION,
                    points: Vec::new(),
                },
            )
            .unwrap();
        let lease = manager.acquire_lease(id).unwrap();
        manager
            .save_rewind_transaction_with_lease(
                &lease,
                &super::super::rewind::RewindTransactionJournal {
                    version: super::super::rewind::TRANSACTION_VERSION,
                    previous_points: Vec::new(),
                    retained_points: Vec::new(),
                    recovery_tree: None,
                    restored_files: Vec::new(),
                    target_snapshot: None,
                    committed: false,
                },
            )
            .unwrap();

        let scan = SessionManager::scan_catalog(root.path());

        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].id, id);
        assert!(
            scan.diagnostics.is_empty(),
            "rewind sidecars must not be parsed as legacy session JSON: {:?}",
            scan.diagnostics
        );
    }

    #[test]
    fn catalog_lookup_rejects_exact_and_prefix_ambiguity_across_buckets() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("1111111111111111");
        let second = root.path().join("2222222222222222");
        write_legacy_catalog_session(&first, "abc-one", "/one", 1);
        write_legacy_catalog_session(&second, "abc-two", "/two", 2);
        write_legacy_catalog_session(&first, "same", "/one", 1);
        write_legacy_catalog_session(&second, "same", "/two", 2);
        let scan = SessionManager::scan_catalog(root.path());

        assert!(matches!(
            scan.find("abc"),
            Err(SessionStoreError::AmbiguousId { ref matches, .. }) if matches.len() == 2
        ));
        assert!(matches!(
            scan.find("same"),
            Err(SessionStoreError::AmbiguousId { ref matches, .. }) if matches.len() == 2
        ));
        assert_eq!(
            scan.find("abc-one").unwrap().unwrap().working_dir,
            PathBuf::from("/one")
        );
        assert!(scan.find("missing").unwrap().is_none());
        assert!(matches!(
            scan.find("../escape"),
            Err(SessionStoreError::InvalidId { .. })
        ));
    }

    #[test]
    fn catalog_reports_orphan_native_sidecars_but_ignores_persistent_locks() {
        let root = tempfile::tempdir().unwrap();
        let bucket = root.path().join("0123456789abcdef");
        let mgr = SessionManager::with_root(&bucket);
        mgr.save_snapshot("orphan", &snap(&["x"])).unwrap();
        let lock_only = mgr.acquire_lease("deleted").unwrap();
        drop(lock_only);

        let scan = SessionManager::scan_catalog(root.path());

        assert!(scan.entries.is_empty());
        assert_eq!(scan.diagnostics.len(), 1);
        assert!(scan.diagnostics[0].path.ends_with("orphan.snapshot"));
        assert_eq!(scan.diagnostics[0].kind, CatalogDiagnosticKind::Corrupt);
    }

    #[test]
    fn logical_delete_requires_the_same_bucket_lease_and_keeps_lock_files() {
        let root = tempfile::tempdir().unwrap();
        let bucket_a = root.path().join("aaaaaaaaaaaaaaaa");
        let bucket_b = root.path().join("bbbbbbbbbbbbbbbb");
        let mgr_a = SessionManager::with_root(&bucket_a);
        let mgr_b = SessionManager::with_root(&bucket_b);
        mgr_b.write_meta(&SessionMeta::new("s1", "/p", 1)).unwrap();
        mgr_b.save_snapshot("s1", &snap(&["x"])).unwrap();
        std::fs::write(mgr_b.jsonl_path("s1").unwrap(), b"{}\n").unwrap();
        mgr_b
            .write_presentation("s1", &PresentationFile::default())
            .unwrap();
        write_legacy_catalog_session(&bucket_b, "s1", "/p", 1);
        let wrong_bucket_lease = mgr_a.acquire_lease("s1").unwrap();

        assert!(matches!(
            mgr_b.delete(&wrong_bucket_lease),
            Err(SessionStoreError::LeaseMismatch { ref id, .. }) if id == "s1"
        ));
        assert!(mgr_b.snapshot_path("s1").unwrap().exists());

        let lease = mgr_b.acquire_lease("s1").unwrap();
        mgr_b.delete(&lease).unwrap();
        assert!(!mgr_b.meta_path("s1").unwrap().exists());
        assert!(!mgr_b.snapshot_path("s1").unwrap().exists());
        assert!(!mgr_b.jsonl_path("s1").unwrap().exists());
        assert!(!mgr_b.presentation_path("s1").unwrap().exists());
        assert!(!bucket_b.join("s1.json").exists());
        assert!(bucket_b.join("s1.lease").exists());
        assert!(bucket_b.join("s1.meta.lock").exists());

        mgr_b.delete(&lease).unwrap();
        let scan = SessionManager::scan_catalog(root.path());
        assert!(scan.entries.is_empty());
        assert!(scan.diagnostics.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn logical_delete_validates_every_target_before_removing_anything() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.save_snapshot("s1", &snap(&["keep"])).unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, b"outside").unwrap();
        symlink(&outside, dir.path().join("s1.json")).unwrap();
        let lease = mgr.acquire_lease("s1").unwrap();

        assert!(matches!(
            mgr.delete(&lease),
            Err(SessionStoreError::UnsafeFile { .. })
        ));
        assert!(mgr.snapshot_path("s1").unwrap().exists());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn delete_removes_all_session_files_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.write_meta(&SessionMeta::new("s1", "/p", 1)).unwrap();
        mgr.save_snapshot("s1", &snap(&["x"])).unwrap();
        std::fs::write(mgr.jsonl_path("s1").unwrap(), b"{}\n").unwrap();
        mgr.write_presentation("s1", &PresentationFile::default())
            .unwrap();

        let lease = mgr.acquire_lease("s1").unwrap();
        mgr.save_rewind_transaction_with_lease(
            &lease,
            &super::super::rewind::RewindTransactionJournal {
                version: super::super::rewind::TRANSACTION_VERSION,
                previous_points: Vec::new(),
                retained_points: Vec::new(),
                recovery_tree: None,
                restored_files: Vec::new(),
                target_snapshot: None,
                committed: false,
            },
        )
        .unwrap();
        mgr.delete(&lease).unwrap();
        assert!(!mgr.meta_path("s1").unwrap().exists());
        assert!(!mgr.snapshot_path("s1").unwrap().exists());
        assert!(!mgr.jsonl_path("s1").unwrap().exists());
        assert!(!mgr.presentation_path("s1").unwrap().exists());
        assert!(!mgr.rewind_transaction_path("s1").unwrap().exists());
        // Idempotent: deleting again is fine.
        mgr.delete(&lease).unwrap();
    }

    #[test]
    fn native_runtime_mutation_commits_snapshot_presentation_then_meta_under_lease() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut original_meta = SessionMeta::new("s1", "/p", 1);
        original_meta.owner = StorageOwner::Native;
        original_meta.message_count = 2;
        mgr.write_meta(&original_meta).unwrap();
        mgr.save_snapshot("s1", &snap(&["before", "removed"]))
            .unwrap();
        mgr.write_presentation(
            "s1",
            &PresentationFile {
                v: super::super::presentation::PRESENTATION_VERSION,
                entries: vec![],
            },
        )
        .unwrap();
        let lease = mgr.acquire_lease("s1").unwrap();
        let next_snapshot = snap(&["after"]);
        let mut next_meta = original_meta;
        next_meta.message_count = 1;
        let next_presentation = PresentationFile::default();

        let receipt = mgr
            .commit_native_runtime_mutation(&lease, &next_snapshot, |_, meta, presentation| {
                *meta = next_meta.clone();
                *presentation = next_presentation.clone();
                Ok("committed")
            })
            .unwrap();

        assert_eq!(receipt, "committed");
        assert_eq!(mgr.load_snapshot("s1").unwrap(), next_snapshot);
        assert_eq!(mgr.read_presentation("s1").unwrap(), next_presentation);
        assert_eq!(mgr.read_meta("s1").unwrap(), next_meta);
    }

    #[test]
    fn invalid_runtime_mutation_writes_none_of_the_three_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        let before_snapshot = snap(&["before"]);
        let before_presentation = PresentationFile::default();
        let lease = mgr.acquire_lease("s1").unwrap();
        mgr.commit_native_import(
            &lease,
            Some(&before_snapshot),
            Some(&before_presentation),
            &meta,
        )
        .unwrap();

        let result = mgr.commit_native_runtime_mutation(
            &lease,
            &snap(&["must not be published"]),
            |_, meta, presentation| {
                meta.id = "different-session".into();
                presentation.entries.push(PresentationEntry {
                    anchor: DisplayAnchor::AtStart,
                    role: PresentationRole::Assistant,
                    text: "must not be published".into(),
                });
                Ok(())
            },
        );

        assert!(matches!(
            result,
            Err(SessionStoreError::Corrupt {
                kind: "session meta",
                ..
            })
        ));
        assert_eq!(mgr.load_snapshot("s1").unwrap(), before_snapshot);
        assert_eq!(mgr.read_presentation("s1").unwrap(), before_presentation);
        assert_eq!(mgr.read_meta("s1").unwrap(), meta);
    }

    #[test]
    fn runtime_mutation_rejects_incomplete_native_aggregate_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        let before_snapshot = snap(&["before"]);
        mgr.write_meta(&meta).unwrap();
        mgr.save_snapshot("s1", &before_snapshot).unwrap();
        let lease = mgr.acquire_lease("s1").unwrap();

        let result = mgr.commit_native_runtime_mutation(
            &lease,
            &snap(&["must not be published"]),
            |_, meta, _presentation| {
                meta.updated_at = 2;
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(mgr.load_snapshot("s1").unwrap(), before_snapshot);
        assert_eq!(mgr.read_meta("s1").unwrap(), meta);
        assert!(!mgr.presentation_path("s1").unwrap().exists());
    }

    #[test]
    fn runtime_mutation_rolls_back_all_preimages_after_presentation_replacement_error() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut meta = SessionMeta::new("s1", "/before", 1);
        meta.owner = StorageOwner::Native;
        let snapshot = snap(&["before"]);
        let presentation = PresentationFile::default();
        mgr.commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        let before = native_artifact_bytes(&mgr, "s1");
        mgr.fail_commit_write(
            CommitArtifact::Presentation,
            CommitFaultTiming::AfterReplace,
        );

        let error = mgr
            .commit_native_runtime_mutation(&lease, &snap(&["after"]), |_, meta, presentation| {
                meta.working_dir = "/after".into();
                presentation
                    .entries
                    .push(presentation_entry(DisplayAnchor::AtStart, "after"));
                Ok(())
            })
            .unwrap_err();

        assert!(matches!(error, SessionStoreError::Io { .. }));
        assert_eq!(native_artifact_bytes(&mgr, "s1"), before);
    }

    #[test]
    fn runtime_mutation_rolls_back_all_preimages_after_meta_replacement_error() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut meta = SessionMeta::new("s1", "/before", 1);
        meta.owner = StorageOwner::Native;
        let snapshot = snap(&["before"]);
        let presentation = PresentationFile::default();
        mgr.commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        mgr.take_commit_write_log();
        let before = native_artifact_bytes(&mgr, "s1");
        mgr.fail_commit_write(CommitArtifact::Meta, CommitFaultTiming::AfterReplace);

        let error = mgr
            .commit_native_runtime_mutation(&lease, &snap(&["after"]), |_, meta, presentation| {
                meta.working_dir = "/after".into();
                presentation
                    .entries
                    .push(presentation_entry(DisplayAnchor::AtStart, "after"));
                Ok(())
            })
            .unwrap_err();

        assert!(matches!(error, SessionStoreError::Io { .. }));
        assert_eq!(native_artifact_bytes(&mgr, "s1"), before);
        assert_eq!(
            mgr.take_commit_write_log(),
            vec![
                CommitArtifact::Snapshot,
                CommitArtifact::Presentation,
                CommitArtifact::Meta,
                CommitArtifact::Meta,
                CommitArtifact::Presentation,
                CommitArtifact::Snapshot,
            ]
        );
    }

    #[test]
    fn runtime_mutation_reports_uncertain_commit_when_rollback_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut meta = SessionMeta::new("s1", "/before", 1);
        meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &lease,
            Some(&snap(&["before"])),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();
        mgr.fail_commit_write(
            CommitArtifact::Presentation,
            CommitFaultTiming::AfterReplace,
        );
        mgr.fail_commit_write(
            CommitArtifact::Presentation,
            CommitFaultTiming::BeforeReplace,
        );

        let error = mgr
            .commit_native_runtime_mutation(&lease, &snap(&["after"]), |_, meta, presentation| {
                meta.working_dir = "/after".into();
                presentation
                    .entries
                    .push(presentation_entry(DisplayAnchor::AtStart, "after"));
                Ok(())
            })
            .unwrap_err();

        assert!(matches!(
            error,
            SessionStoreError::UncertainCommit {
                ref rollback_errors,
                ..
            } if !rollback_errors.is_empty()
        ));
    }

    #[test]
    fn runtime_mutation_does_not_rewrite_unchanged_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut meta = SessionMeta::new("s1", "/before", 1);
        meta.owner = StorageOwner::Native;
        let snapshot = snap(&["same"]);
        mgr.commit_native_import(
            &lease,
            Some(&snapshot),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();
        mgr.take_commit_write_log();

        mgr.commit_native_runtime_mutation(&lease, &snapshot, |_, meta, _presentation| {
            meta.updated_at = 2;
            Ok(())
        })
        .unwrap();

        assert_eq!(mgr.take_commit_write_log(), vec![CommitArtifact::Meta]);
    }

    #[test]
    fn snapshot_write_and_native_aggregate_load_wait_for_meta_lock() {
        use std::sync::mpsc::{self, RecvTimeoutError};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(SessionManager::with_root(dir.path()));
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &lease,
            Some(&snap(&["before"])),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();

        let (lock_entered_tx, lock_entered_rx) = mpsc::channel();
        let (release_lock_tx, release_lock_rx) = mpsc::channel();
        let lock_mgr = Arc::clone(&mgr);
        let lock_thread = std::thread::spawn(move || {
            lock_mgr
                .with_meta_lock("s1", || {
                    lock_entered_tx.send(()).unwrap();
                    release_lock_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
        });
        lock_entered_rx.recv().unwrap();

        let (save_started_tx, save_started_rx) = mpsc::channel();
        let (save_done_tx, save_done_rx) = mpsc::channel();
        let save_mgr = Arc::clone(&mgr);
        let save_thread = std::thread::spawn(move || {
            save_started_tx.send(()).unwrap();
            let result = save_mgr.save_snapshot("s1", &snap(&["after"]));
            save_done_tx.send(result).unwrap();
        });
        save_started_rx.recv().unwrap();
        assert!(matches!(
            save_done_rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));

        let (load_started_tx, load_started_rx) = mpsc::channel();
        let (load_done_tx, load_done_rx) = mpsc::channel();
        let load_mgr = Arc::clone(&mgr);
        let load_thread = std::thread::spawn(move || {
            load_started_tx.send(()).unwrap();
            let result = load_mgr.load_native_session("s1");
            load_done_tx.send(result).unwrap();
        });
        load_started_rx.recv().unwrap();
        assert!(matches!(
            load_done_rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));

        release_lock_tx.send(()).unwrap();
        lock_thread.join().unwrap();
        save_done_rx.recv().unwrap().unwrap();
        load_done_rx.recv().unwrap().unwrap();
        save_thread.join().unwrap();
        load_thread.join().unwrap();
    }

    #[test]
    fn bounded_meta_lock_wait_returns_contention() {
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.meta.lock");
        let held = super::open_lock_file(&path).unwrap();
        fs2::FileExt::try_lock_exclusive(&held).unwrap();
        let contender = super::open_lock_file(&path).unwrap();

        let started = Instant::now();
        let error = super::acquire_file_lock_until(&contender, Duration::from_millis(40))
            .expect_err("contended lock must stop at its deadline");
        assert!(super::is_file_lock_contention(&error));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn update_meta_returns_fresh_state_decision_without_overwriting_user_rename() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        mgr.write_meta(&meta).unwrap();
        mgr.rename("s1", "User title").unwrap();

        let renamed = mgr
            .update_meta("s1", |meta| {
                if meta.user_renamed || meta.ai_named {
                    false
                } else {
                    meta.name = "AI title".into();
                    meta.ai_named = true;
                    true
                }
            })
            .unwrap();

        assert!(!renamed);
        let meta = mgr.read_meta("s1").unwrap();
        assert_eq!(meta.name, "User title");
        assert!(meta.user_renamed);
        assert!(!meta.ai_named);
    }

    #[test]
    fn update_meta_cannot_change_storage_owner() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        mgr.write_meta(&meta).unwrap();

        assert!(matches!(
            mgr.update_meta("s1", |meta| meta.owner = StorageOwner::Legacy),
            Err(SessionStoreError::OwnershipConflict {
                owner: StorageOwner::Legacy,
                ..
            })
        ));
        assert_eq!(mgr.read_meta("s1").unwrap(), meta);
    }

    #[test]
    fn stale_sidecar_repair_does_not_overwrite_concurrent_native_updates() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        let snapshot = snap(&["native"]);
        let presentation = PresentationFile::default();
        let lease = mgr.acquire_lease("s1").unwrap();
        mgr.commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        let expected_meta = mgr.read_meta("s1").unwrap();
        let expected_presentation = mgr.read_presentation("s1").unwrap();
        let mut repaired_meta = expected_meta.clone();
        repaired_meta.updated_at = 2;

        mgr.rename("s1", "user rename").unwrap();
        mgr.append_presentation_at_latest_valid_turn(
            "s1",
            &[(PresentationRole::User, "concurrent append".into())],
        )
        .unwrap();

        let committed = mgr
            .commit_native_sidecar_repair_if_unchanged(
                &lease,
                &expected_meta,
                &snapshot,
                &expected_presentation,
                &repaired_meta,
            )
            .unwrap();

        assert!(!committed);
        assert_eq!(mgr.read_meta("s1").unwrap().name, "user rename");
        assert_eq!(
            mgr.read_presentation("s1").unwrap().entries[0].text,
            "concurrent append"
        );
    }

    #[test]
    fn stale_sidecar_repair_does_not_commit_after_snapshot_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        let original_snapshot = snap(&["original"]);
        let presentation = PresentationFile::default();
        let lease = mgr.acquire_lease("s1").unwrap();
        mgr.commit_native_import(&lease, Some(&original_snapshot), Some(&presentation), &meta)
            .unwrap();
        let mut repaired_meta = meta.clone();
        repaired_meta.updated_at = 2;

        mgr.save_snapshot("s1", &snap(&["concurrent"])).unwrap();

        let committed = mgr
            .commit_native_sidecar_repair_if_unchanged(
                &lease,
                &meta,
                &original_snapshot,
                &presentation,
                &repaired_meta,
            )
            .unwrap();

        assert!(!committed);
        assert_eq!(mgr.read_meta("s1").unwrap(), meta);
    }

    #[test]
    fn native_sidecar_repair_writes_only_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        let presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![presentation_entry(DisplayAnchor::AtStart, "keep")],
        };
        let snapshot = snap(&["native"]);
        mgr.commit_native_import(&lease, Some(&snapshot), Some(&presentation), &meta)
            .unwrap();
        let presentation_bytes = std::fs::read(mgr.presentation_path("s1").unwrap()).unwrap();
        let mut repaired_meta = meta.clone();
        repaired_meta.updated_at = 2;

        assert!(mgr
            .commit_native_sidecar_repair_if_unchanged(
                &lease,
                &meta,
                &snapshot,
                &presentation,
                &repaired_meta,
            )
            .unwrap());

        assert_eq!(mgr.read_meta("s1").unwrap(), repaired_meta);
        assert_eq!(
            std::fs::read(mgr.presentation_path("s1").unwrap()).unwrap(),
            presentation_bytes
        );
    }

    #[test]
    fn legacy_import_intent_is_durable_before_sidecar_publication() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut intent = SessionMeta::new("s1", "/legacy", 1);
        intent.owner = StorageOwner::Legacy;
        let mut invalid_intent = intent.clone();
        invalid_intent.import_info = Some(ImportInfo {
            legacy_schema: "legacy".into(),
            source_sha256: "0".repeat(64),
            importer_version: 1,
            kind: ImportKind::Full,
        });

        assert!(matches!(
            mgr.begin_legacy_import(&lease, &invalid_intent),
            Err(SessionStoreError::Corrupt {
                kind: "session import",
                ..
            })
        ));
        assert!(!mgr.meta_path("s1").unwrap().exists());

        mgr.begin_legacy_import(&lease, &intent).unwrap();

        assert_eq!(mgr.read_meta("s1").unwrap(), intent);
        assert!(!mgr.snapshot_path("s1").unwrap().exists());
        assert!(!mgr.presentation_path("s1").unwrap().exists());
    }

    #[test]
    fn legacy_import_intent_refuses_to_replace_native_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut native = SessionMeta::new("s1", "/native", 1);
        native.owner = StorageOwner::Native;
        mgr.write_meta(&native).unwrap();
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut intent = SessionMeta::new("s1", "/legacy", 2);
        intent.owner = StorageOwner::Legacy;

        assert!(matches!(
            mgr.begin_legacy_import(&lease, &intent),
            Err(SessionStoreError::OwnershipConflict {
                owner: StorageOwner::Native,
                ..
            })
        ));
        assert_eq!(mgr.read_meta("s1").unwrap(), native);
    }

    #[test]
    fn legacy_import_intent_cas_refuses_to_downgrade_native_aggregate() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let snapshot = snap(&["native"]);
        let presentation = PresentationFile::default();
        let mut native = SessionMeta::new("s1", "/native", 1);
        native.owner = StorageOwner::Native;
        mgr.commit_native_import(&lease, Some(&snapshot), Some(&presentation), &native)
            .unwrap();
        let original_meta = std::fs::read(mgr.meta_path("s1").unwrap()).unwrap();
        let original_snapshot = std::fs::read(mgr.snapshot_path("s1").unwrap()).unwrap();
        let original_presentation = std::fs::read(mgr.presentation_path("s1").unwrap()).unwrap();
        let mut intent = native.clone();
        intent.owner = StorageOwner::Legacy;

        assert!(matches!(
            mgr.begin_legacy_import_if_unchanged(
                &lease,
                Some(&native),
                Some(&snapshot),
                Some(&presentation),
                &intent,
            ),
            Err(SessionStoreError::OwnershipConflict {
                owner: StorageOwner::Native,
                ..
            })
        ));
        assert_eq!(
            std::fs::read(mgr.meta_path("s1").unwrap()).unwrap(),
            original_meta
        );
        assert_eq!(
            std::fs::read(mgr.snapshot_path("s1").unwrap()).unwrap(),
            original_snapshot
        );
        assert_eq!(
            std::fs::read(mgr.presentation_path("s1").unwrap()).unwrap(),
            original_presentation
        );
    }

    #[test]
    fn legacy_import_intent_refuses_unconfirmed_without_expected_state_cas() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let original = SessionMeta::new("s1", "/native", 1);
        mgr.write_meta(&original).unwrap();
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut intent = original.clone();
        intent.owner = StorageOwner::Legacy;

        mgr.rename("s1", "concurrent rename").unwrap();

        assert!(matches!(
            mgr.begin_legacy_import(&lease, &intent),
            Err(SessionStoreError::OwnershipConflict {
                owner: StorageOwner::Unconfirmed,
                ..
            })
        ));
        assert_eq!(mgr.read_meta("s1").unwrap().name, "concurrent rename");
    }

    #[test]
    fn legacy_import_intent_cas_preserves_concurrent_unconfirmed_state() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let original = SessionMeta::new("s1", "/native", 1);
        mgr.write_meta(&original).unwrap();
        let lease = mgr.acquire_lease("s1").unwrap();
        let mut stale_intent = original.clone();
        stale_intent.owner = StorageOwner::Legacy;

        mgr.rename("s1", "concurrent rename").unwrap();

        assert!(!mgr
            .begin_legacy_import_if_unchanged(&lease, Some(&original), None, None, &stale_intent,)
            .unwrap());
        let renamed = mgr.read_meta("s1").unwrap();
        assert_eq!(renamed.name, "concurrent rename");
        let mut fresh_intent = renamed.clone();
        fresh_intent.owner = StorageOwner::Legacy;
        assert!(mgr
            .begin_legacy_import_if_unchanged(&lease, Some(&renamed), None, None, &fresh_intent,)
            .unwrap());
        assert_eq!(mgr.read_meta("s1").unwrap(), fresh_intent);
    }

    #[test]
    fn catalog_presentation_append_selects_latest_valid_turn_under_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 3;
        meta.turn_stats = vec![
            TurnStat {
                after_message: 2,
                position_valid: true,
                turn_id: 7,
                round_count: 1,
                tool_call_count: 0,
                duration_ms: 1,
                total_tokens: 1,
                errored: false,
                used_tokens: 1,
                ctx_window: 10,
                model_usage: Vec::new(),
            },
            TurnStat {
                after_message: 99,
                position_valid: false,
                turn_id: 8,
                round_count: 1,
                tool_call_count: 0,
                duration_ms: 1,
                total_tokens: 1,
                errored: false,
                used_tokens: 1,
                ctx_window: 10,
                model_usage: Vec::new(),
            },
        ];
        mgr.write_meta(&meta).unwrap();
        mgr.write_presentation("s1", &PresentationFile::default())
            .unwrap();

        let count = mgr
            .append_presentation_at_latest_valid_turn(
                "s1",
                &[(PresentationRole::Assistant, "note".into())],
            )
            .unwrap();

        assert_eq!(count, 4);
        assert_eq!(
            mgr.read_presentation("s1").unwrap().entries[0].anchor,
            DisplayAnchor::AfterTurn { turn_id: 7 }
        );
    }

    #[test]
    fn catalog_presentation_append_rejects_missing_native_presentation() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.owner = StorageOwner::Native;
        mgr.write_meta(&meta).unwrap();

        let error = mgr
            .append_presentation_at_latest_valid_turn(
                "s1",
                &[(PresentationRole::Assistant, "note".into())],
            )
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!mgr.presentation_path("s1").unwrap().exists());
    }

    #[test]
    fn active_lease_rejects_second_owner_until_last_clone_drops() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let lease = mgr.acquire_lease("s1").unwrap();
        let lease_clone = lease.clone();

        assert!(matches!(
            mgr.acquire_lease("s1"),
            Err(SessionStoreError::SessionInUse { ref id, .. }) if id == "s1"
        ));
        drop(lease);
        assert!(matches!(
            mgr.acquire_lease("s1"),
            Err(SessionStoreError::SessionInUse { .. })
        ));

        drop(lease_clone);
        mgr.acquire_lease("s1").unwrap();
    }

    #[test]
    fn delete_requires_the_active_session_lease() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.save_snapshot("s1", &snap(&["keep"])).unwrap();
        let lease = mgr.acquire_lease("s1").unwrap();

        assert!(matches!(
            mgr.acquire_lease("s1"),
            Err(SessionStoreError::SessionInUse { ref id, .. }) if id == "s1"
        ));
        assert!(mgr.snapshot_path("s1").unwrap().exists());

        mgr.delete(&lease).unwrap();
        assert!(!mgr.snapshot_path("s1").unwrap().exists());
    }

    #[test]
    fn delete_removes_artifacts_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.save_snapshot("s1", &snap(&["keep"])).unwrap();
        let lease = mgr.acquire_lease("s1").unwrap();
        let art = mgr.artifacts_dir("s1").unwrap();
        std::fs::create_dir_all(&art).unwrap();
        std::fs::write(art.join("deadbeefdeadbeef"), b"x").unwrap();
        assert!(art.exists());
        mgr.delete(&lease).unwrap();
        assert!(!art.exists(), "artifacts dir removed with the session");
    }

    #[cfg(unix)]
    #[test]
    fn active_lease_rejects_a_symlink_lock_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let target = dir.path().join("target");
        std::fs::write(&target, b"do not follow").unwrap();
        symlink(&target, mgr.lease_path("s1").unwrap()).unwrap();

        assert!(matches!(
            mgr.acquire_lease("s1"),
            Err(SessionStoreError::UnsafeFile { .. })
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"do not follow");
    }

    #[test]
    fn atomic_write_leaves_no_tmp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.save_snapshot("s1", &snap(&["x"])).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .tmp must survive a successful write"
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_writers_create_and_repair_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        fn mode(path: &Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        let dir = tempfile::tempdir().unwrap();

        let atomic_path = dir.path().join("private.snapshot");
        atomic_write(&atomic_path, b"first").unwrap();
        assert_eq!(mode(&atomic_path), 0o600);
        std::fs::set_permissions(&atomic_path, fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write(&atomic_path, b"second").unwrap();
        assert_eq!(
            mode(&atomic_path),
            0o600,
            "atomic replacement must tighten an existing session artifact"
        );

        let transcript_path = dir.path().join("private.jsonl");
        drop(open_append_file(&transcript_path).unwrap());
        assert_eq!(mode(&transcript_path), 0o600);
        std::fs::set_permissions(&transcript_path, fs::Permissions::from_mode(0o644)).unwrap();
        drop(open_append_file(&transcript_path).unwrap());
        assert_eq!(
            mode(&transcript_path),
            0o600,
            "opening an existing transcript for append must tighten it before writing"
        );

        let lock_path = dir.path().join("private.lease");
        drop(open_lock_file(&lock_path).unwrap());
        assert_eq!(mode(&lock_path), 0o600);
        std::fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();
        drop(open_lock_file(&lock_path).unwrap());
        assert_eq!(
            mode(&lock_path),
            0o600,
            "opening an existing session lock must tighten it before use"
        );
    }

    #[test]
    fn session_tokens_group_by_provider_and_model_without_relabeling_legacy_usage() {
        let mut meta = SessionMeta::new("cost", "/project", 1);
        let stat = |turn_id, usage: Vec<ModelUsageStat>, legacy_total| TurnStat {
            after_message: turn_id as usize * 2,
            position_valid: true,
            turn_id,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: legacy_total,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
            model_usage: usage,
        };
        meta.turn_stats.push(stat(
            1,
            vec![ModelUsageStat {
                provider_id: "provider-a".into(),
                model_id: "same-name".into(),
                tokens: TokenBreakdown {
                    input: 100,
                    output: 20,
                    cached_input: 50,
                },
            }],
            170,
        ));
        meta.turn_stats.push(stat(
            2,
            vec![ModelUsageStat {
                provider_id: "provider-b".into(),
                model_id: "same-name".into(),
                tokens: TokenBreakdown {
                    input: 10,
                    output: 0,
                    cached_input: 0,
                },
            }],
            10,
        ));
        meta.turn_stats.push(stat(3, Vec::new(), 40));

        let report = aggregate_session_cost(&meta);
        assert_eq!(report.models.len(), 2);
        assert_eq!(report.models[0].provider_id, "provider-a");
        assert_eq!(report.models[0].tokens.total(), 170);
        assert_eq!(report.models[1].provider_id, "provider-b");
        assert_eq!(report.models[1].tokens.total(), 10);
        assert_eq!(report.unattributed_tokens, 40);
        assert_eq!(report.total_tokens, 220);
    }

    #[test]
    fn legacy_model_usage_pricing_is_ignored_and_not_rewritten() {
        let source = r#"{
            "provider_id": "provider-a",
            "model_id": "model-a",
            "tokens": {"input": 10, "output": 2, "cached_input": 3},
            "pricing": {
                "input_per_million": 1.0,
                "output_per_million": 2.0,
                "cached_input_per_million": 0.5
            }
        }"#;

        let usage: ModelUsageStat = serde_json::from_str(source).unwrap();
        assert_eq!(usage.tokens.total(), 15);
        let rendered = serde_json::to_string(&usage).unwrap();
        assert!(!rendered.contains("pricing"));
        assert!(!rendered.contains("per_million"));
    }

    #[test]
    fn detached_model_usage_is_aggregated_without_changing_turn_count() {
        let mut meta = SessionMeta::new("s1", "/p", 1);
        meta.detached_model_usage.push(ModelUsageStat {
            provider_id: "fast".into(),
            model_id: "fast-model".into(),
            tokens: TokenBreakdown {
                input: 10,
                output: 2,
                cached_input: 3,
            },
        });

        let report = aggregate_session_cost(&meta);
        assert_eq!(meta.turn_count, 0);
        assert_eq!(report.total_tokens, 15);
        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].provider_id, "fast");
        assert_eq!(report.models[0].tokens.total(), 15);
    }

    #[test]
    fn archiving_turn_stats_preserves_attributed_and_legacy_tokens() {
        let mut meta = SessionMeta::new("archive", "/p", 1);
        meta.turn_stats.push(TurnStat {
            after_message: 2,
            position_valid: true,
            turn_id: 1,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 12,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
            model_usage: vec![ModelUsageStat {
                provider_id: "p".into(),
                model_id: "m".into(),
                tokens: TokenBreakdown {
                    input: 10,
                    output: 2,
                    cached_input: 0,
                },
            }],
        });
        meta.turn_stats.push(TurnStat {
            after_message: 4,
            position_valid: true,
            turn_id: 2,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 7,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
            model_usage: Vec::new(),
        });

        let archived = meta.archive_turn_stats_where(|_| true);

        assert_eq!(archived.len(), 2);
        assert!(meta.turn_stats.is_empty());
        assert_eq!(meta.detached_unattributed_tokens, 7);
        let report = aggregate_session_cost(&meta);
        assert_eq!(report.models[0].tokens.total(), 12);
        assert_eq!(report.unattributed_tokens, 7);
        assert_eq!(report.total_tokens, 19);

        merge_model_usage(
            &mut meta.detached_model_usage,
            ModelUsageStat {
                provider_id: "p".into(),
                model_id: "m".into(),
                tokens: TokenBreakdown {
                    input: 1,
                    output: 0,
                    cached_input: 0,
                },
            },
        );
        meta.detached_unattributed_tokens += 3;
        meta.remove_archived_turn_usage(&archived);
        let concurrent_only = aggregate_session_cost(&meta);
        assert_eq!(concurrent_only.models[0].tokens.total(), 1);
        assert_eq!(concurrent_only.unattributed_tokens, 3);
        assert_eq!(concurrent_only.total_tokens, 4);
    }

    #[test]
    fn resume_recovers_only_one_user_message_beyond_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let id = "recover-user-prompt";
        let lease = mgr.acquire_lease(id).unwrap();
        let canonical = snap(&["completed"]);
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &lease,
            Some(&canonical),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();
        let inflight = snap(&["completed", "accepted before crash"]);
        mgr.save_inflight_snapshot(id, &inflight, true).unwrap();

        assert_eq!(
            mgr.load_native_session(id).unwrap().snapshot,
            canonical,
            "general native loads remain canonical"
        );
        assert_eq!(
            mgr.load_native_session_for_resume(&lease).unwrap(),
            (
                LoadedSession {
                    meta: mgr.read_meta(id).unwrap(),
                    snapshot: canonical,
                    presentation: PresentationFile::default(),
                },
                inflight.messages.last().cloned(),
            ),
            "the leased resume boundary returns canonical history plus a prompt to replay"
        );
    }

    #[test]
    fn resume_rejects_inflight_assistant_state() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let id = "reject-unsafe-inflight";
        let lease = mgr.acquire_lease(id).unwrap();
        let canonical = snap(&["completed"]);
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &lease,
            Some(&canonical),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();
        let unsafe_inflight = SessionSnapshot::new(vec![
            Message::user("completed"),
            Message::assistant("tool round not committed", Vec::new()),
        ]);
        mgr.save_inflight_snapshot(id, &unsafe_inflight, true)
            .unwrap();

        assert_eq!(
            mgr.load_native_session_for_resume(&lease).unwrap(),
            (
                LoadedSession {
                    meta: mgr.read_meta(id).unwrap(),
                    snapshot: canonical,
                    presentation: PresentationFile::default(),
                },
                None,
            )
        );
        assert!(
            !mgr.has_inflight_snapshot(id),
            "unsafe checkpoints are cleared so they cannot shadow later resumes"
        );
    }

    #[test]
    fn resume_does_not_replay_after_model_processing_started() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let id = "recover-without-replay";
        let lease = mgr.acquire_lease(id).unwrap();
        let canonical = snap(&["completed"]);
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &lease,
            Some(&canonical),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();
        let inflight = snap(&["completed", "possibly executed"]);
        mgr.save_inflight_snapshot_with_lease(&lease, &inflight, true)
            .unwrap();
        mgr.mark_inflight_not_replayable(id).unwrap();

        let (loaded, pending) = mgr.load_native_session_for_resume(&lease).unwrap();

        assert_eq!(loaded.snapshot, inflight);
        assert_eq!(pending, None);
    }

    #[test]
    fn delete_removes_the_inflight_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let id = "delete-inflight";
        let lease = mgr.acquire_lease(id).unwrap();
        let snapshot = snap(&["accepted"]);
        let mut meta = SessionMeta::new(id, "/project", 1);
        meta.owner = StorageOwner::Native;
        mgr.commit_native_import(
            &lease,
            Some(&snapshot),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();
        mgr.save_inflight_snapshot_with_lease(&lease, &snapshot, true)
            .unwrap();

        mgr.delete(&lease).unwrap();

        assert!(!mgr.has_inflight_snapshot(id));
    }

    #[test]
    fn artifacts_dir_is_sibling_of_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        let snap = mgr.snapshot_path("abc").unwrap();
        let art = mgr.artifacts_dir("abc").unwrap();
        assert_eq!(art, snap.with_extension("artifacts"));
    }

    #[test]
    fn session_origin_defaults_manual_and_roundtrips() {
        let mut m = SessionMeta::new("s1", "/tmp/p", 0);
        assert_eq!(m.origin, SessionOrigin::Manual); // default
        m.origin = SessionOrigin::Scheduled;
        let json = serde_json::to_string(&m).unwrap();
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.origin, SessionOrigin::Scheduled);
        // old meta without the field → Manual
        let old = r#"{"id":"x","name":"n","working_dir":"/w","created_at":0,"updated_at":0}"#;
        let parsed: SessionMeta = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.origin, SessionOrigin::Manual);
    }
}
