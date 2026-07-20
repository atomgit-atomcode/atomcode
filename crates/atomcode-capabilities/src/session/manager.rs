//! `SessionManager` — the two-tier on-disk session store + its fast-listing metadata.
//!
//! Pure storage: no kernel coupling beyond serializing the kernel's `SessionSnapshot`.
//! The hooks (snapshot / transcript) and the recall tool call into this; the manager
//! itself does only file IO, so it is fully unit-testable with a temp root.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Barrier, Mutex};

use atomcode_kernel::message::{SessionSnapshot, SNAPSHOT_VERSION};
use serde::{de::IgnoredAny, Deserialize, Serialize};

use super::presentation::{PresentationEntry, PresentationFile, MAX_PRESENTATION_BYTES};

/// Fast-listing metadata for ONE session — read to populate a `/resume` picker WITHOUT
/// parsing the (large) snapshot / transcript files. Persisted as `<id>.meta`.
pub const META_VERSION: u32 = 1;

// These are persistence safety ceilings, not product quotas. They are deliberately
// above normal context-window payloads, but finite so a corrupted/untrusted session file
// cannot force an unbounded allocation before serde gets a chance to reject it.
pub const MAX_SESSION_ID_BYTES: usize = 128;
pub const MAX_META_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
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
}

impl SessionStoreError {
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

#[derive(Debug, Default)]
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
            working_dir: working_dir.into(),
            created_at: now_ms,
            updated_at: now_ms,
            turn_count: 0,
            message_count: 0,
            turn_stats: Vec::new(),
        }
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
}

/// The per-project session store at `$ATOMCODE_HOME/sessions/<project_hash>/`.
pub struct SessionManager {
    root: PathBuf,
    #[cfg(test)]
    meta_read_pause: Mutex<Option<Arc<MetaReadPause>>>,
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
        }
    }

    /// Point the store at an explicit directory (tests / custom layouts).
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            #[cfg(test)]
            meta_read_pause: Mutex::new(None),
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

    pub fn legacy_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "json")
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
        let file = open_lock_file(&path)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(SessionLease {
                inner: Arc::new(SessionLeaseInner {
                    id: id.to_string(),
                    path,
                    file,
                }),
            }),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Err(SessionStoreError::SessionInUse {
                    id: id.to_string(),
                    path,
                })
            }
            Err(error) => Err(io_at(&path, error)),
        }
    }

    /// Persist the working-set snapshot (atomic). Overwrites every turn.
    pub fn save_snapshot(&self, id: &str, snap: &SessionSnapshot) -> SessionResult<()> {
        self.ensure_native_writable(id, "save snapshot")?;
        validate_snapshot(snap)?;
        let bytes = serialize_bounded(snap, "snapshot", MAX_SNAPSHOT_BYTES)?;
        atomic_write(&self.snapshot_path(id)?, &bytes)
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
    pub fn update_meta(
        &self,
        id: &str,
        update: impl FnOnce(&mut SessionMeta),
    ) -> SessionResult<()> {
        self.with_meta_lock(id, || {
            let mut meta = self.read_meta(id)?;
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
        let lock = open_lock_file(&lock_path)?;
        fs2::FileExt::lock_exclusive(&lock).map_err(|e| io_at(&lock_path, e))?;
        operation()
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
            if let Some(bytes) = &snapshot_bytes {
                atomic_write(&self.snapshot_path(lease.id())?, bytes)?;
            }
            if let Some(bytes) = &presentation_bytes {
                atomic_write(&self.presentation_path(lease.id())?, bytes)?;
            }
            atomic_write(&self.meta_path(lease.id())?, &meta_bytes)
        })
    }

    /// Commit an owner-native runtime mutation under the active session lease.
    /// All payloads are validated and serialized before the first replacement;
    /// metadata is written last and is the catalog-visible commit point.
    pub fn commit_native_runtime_mutation(
        &self,
        lease: &SessionLease,
        snapshot: &SessionSnapshot,
        presentation: &PresentationFile,
        meta: &SessionMeta,
    ) -> SessionResult<()> {
        self.validate_lease(lease)?;
        if meta.id != lease.id() {
            return Err(SessionStoreError::Corrupt {
                kind: "session mutation",
                message: format!(
                    "lease id {:?} does not match metadata id {:?}",
                    lease.id(),
                    meta.id
                ),
            });
        }
        if meta.owner != StorageOwner::Native {
            return Err(SessionStoreError::OwnershipConflict {
                id: meta.id.clone(),
                owner: meta.owner.clone(),
                operation: "commit native runtime mutation",
            });
        }
        validate_snapshot(snapshot)?;
        presentation.validate()?;
        validate_meta(meta)?;
        let snapshot_bytes = serialize_bounded(snapshot, "snapshot", MAX_SNAPSHOT_BYTES)?;
        let presentation_bytes =
            serialize_pretty_bounded(presentation, "presentation", MAX_PRESENTATION_BYTES)?;
        let meta_bytes = serialize_pretty_bounded(meta, "session meta", MAX_META_BYTES)?;

        self.with_meta_lock(lease.id(), || {
            let current = self.read_meta(lease.id())?;
            if current.owner != StorageOwner::Native {
                return Err(SessionStoreError::OwnershipConflict {
                    id: lease.id().to_string(),
                    owner: current.owner,
                    operation: "commit native runtime mutation",
                });
            }
            atomic_write(&self.snapshot_path(lease.id())?, &snapshot_bytes)?;
            atomic_write(&self.presentation_path(lease.id())?, &presentation_bytes)?;
            atomic_write(&self.meta_path(lease.id())?, &meta_bytes)
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
        scan_catalog_root(sessions_root)
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
        let mut file = open_append_file(&path)?;
        fs2::FileExt::lock_exclusive(&file).map_err(|e| io_at(&path, e))?;
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
                .or_else(|| name.strip_suffix(".jsonl"));
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
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(target)
            .map_err(|error| io_at(target, error))?;
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
        | SessionStoreError::Io { .. } => CatalogDiagnosticKind::Io,
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
    options.create(true).append(true);
    no_follow(&mut options);
    let file = options.open(path).map_err(|e| io_at(path, e))?;
    ensure_opened_regular(path, &file)?;
    Ok(file)
}

fn open_lock_file(path: &Path) -> SessionResult<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    no_follow(&mut options);
    let file = options.open(path).map_err(|e| io_at(path, e))?;
    ensure_opened_regular(path, &file)?;
    Ok(file)
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
/// target, so a crash mid-write never leaves a half-written (corrupt) session file. The
/// tmp's extension (`…tmp`) is ignored by [`SessionManager::list`]'s `*.meta` filter,
/// so a leftover tmp from a crash never appears as a session.
fn atomic_write(path: &Path, bytes: &[u8]) -> SessionResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_at(parent, e))?;
    }
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
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| io_at(&tmp, e))?;
        file.write_all(bytes).map_err(|e| io_at(&tmp, e))?;
        file.sync_all().map_err(|e| io_at(&tmp, e))?;
        fs::rename(&tmp, path).map_err(|e| io_at(path, e))
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
            turn_id,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
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
        assert_eq!(rewritten["turn_stats"][0]["used_tokens"], 0);
        assert_eq!(rewritten["turn_stats"][0]["ctx_window"], 0);
        assert_eq!(meta.turn_stats[0].turn_id, 0);
        assert_eq!(rewritten["turn_stats"][0]["turn_id"], 0);
        assert_eq!(meta.owner, StorageOwner::Unconfirmed);
        assert!(meta.import_info.is_none());
        assert_eq!(rewritten["owner"], "unconfirmed");
        assert!(rewritten["import_info"].is_null());
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
        mgr.write_meta(&SessionMeta::new("s1", "/p", 1)).unwrap();
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
        mgr.delete(&lease).unwrap();
        assert!(!mgr.meta_path("s1").unwrap().exists());
        assert!(!mgr.snapshot_path("s1").unwrap().exists());
        assert!(!mgr.jsonl_path("s1").unwrap().exists());
        assert!(!mgr.presentation_path("s1").unwrap().exists());
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

        mgr.commit_native_runtime_mutation(&lease, &next_snapshot, &next_presentation, &next_meta)
            .unwrap();

        assert_eq!(mgr.load_snapshot("s1").unwrap(), next_snapshot);
        assert_eq!(mgr.read_presentation("s1").unwrap(), next_presentation);
        assert_eq!(mgr.read_meta("s1").unwrap(), next_meta);
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
}
