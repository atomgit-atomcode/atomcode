//! `SessionManager` — the two-tier on-disk session store + its fast-listing metadata.
//!
//! Pure storage: no kernel coupling beyond serializing the kernel's `SessionSnapshot`.
//! The hooks (snapshot / transcript) and the recall tool call into this; the manager
//! itself does only file IO, so it is fully unit-testable with a temp root.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Barrier, Mutex};

use atomcode_kernel::message::{SessionSnapshot, SNAPSHOT_VERSION};
use serde::{Deserialize, Serialize};

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
            working_dir: working_dir.into(),
            created_at: now_ms,
            updated_at: now_ms,
            turn_count: 0,
            message_count: 0,
            turn_stats: Vec::new(),
        }
    }
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
    /// The store for `working_dir`'s project — `$ATOMCODE_HOME/sessions/<project_hash>`,
    /// the SAME bucket production uses (so old `<id>.json` and new `<id>.snapshot`
    /// sessions of the same project land together).
    pub fn for_project(working_dir: &Path) -> Self {
        let root = super::config_dir()
            .join("sessions")
            .join(Self::project_hash(working_dir));
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

    /// Stable per-project bucket id, BYTE-FOR-BYTE the production scheme
    /// (`atomcode_core::session::hash_path`): normalize the path (backslashes→slashes,
    /// strip one trailing slash, lowercase on Windows), then hash the resulting
    /// `PathBuf` — NOT the `&str` — with the std `DefaultHasher`, formatted `{:016x}`.
    /// Hashing the `PathBuf` (length-prefixed components) rather than the string is
    /// what keeps us on production's bucket so a future unified `/resume` still finds
    /// legacy sessions.
    pub fn project_hash(working_dir: &Path) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let normalized = working_dir.to_string_lossy();
        let mut normalized = normalized.replace('\\', "/");
        if normalized.len() > 1 && normalized.ends_with('/') {
            normalized.pop();
        }
        #[cfg(windows)]
        let normalized = normalized.to_lowercase();

        let mut hasher = DefaultHasher::new();
        let p: PathBuf = PathBuf::from(normalized);
        p.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
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
        self.with_meta_lock(&meta.id, || self.write_meta_unlocked(meta))
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
        self.with_meta_lock(id, || self.write_presentation_unlocked(id, presentation))
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

    pub fn append_presentation(&self, id: &str, entry: PresentationEntry) -> SessionResult<()> {
        self.with_meta_lock(id, || {
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

    /// Remove a session's persisted files. A missing file is not an error (idempotent).
    pub fn delete(&self, id: &str) -> SessionResult<()> {
        let _lease = self.acquire_lease(id)?;
        for p in [
            self.snapshot_path(id)?,
            self.meta_path(id)?,
            self.jsonl_path(id)?,
            self.presentation_path(id)?,
        ] {
            match fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_at(&p, e)),
            }
        }
        Ok(())
    }

    pub(crate) fn append_jsonl_line(&self, id: &str, line: &[u8]) -> SessionResult<()> {
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

    #[test]
    fn delete_removes_all_session_files_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.write_meta(&SessionMeta::new("s1", "/p", 1)).unwrap();
        mgr.save_snapshot("s1", &snap(&["x"])).unwrap();
        std::fs::write(mgr.jsonl_path("s1").unwrap(), b"{}\n").unwrap();
        mgr.write_presentation("s1", &PresentationFile::default())
            .unwrap();

        mgr.delete("s1").unwrap();
        assert!(!mgr.meta_path("s1").unwrap().exists());
        assert!(!mgr.snapshot_path("s1").unwrap().exists());
        assert!(!mgr.jsonl_path("s1").unwrap().exists());
        assert!(!mgr.presentation_path("s1").unwrap().exists());
        // Idempotent: deleting again is fine.
        mgr.delete("s1").unwrap();
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
    fn delete_rejects_an_active_session_without_removing_data() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.save_snapshot("s1", &snap(&["keep"])).unwrap();
        let lease = mgr.acquire_lease("s1").unwrap();

        assert!(matches!(
            mgr.delete("s1"),
            Err(SessionStoreError::SessionInUse { ref id, .. }) if id == "s1"
        ));
        assert!(mgr.snapshot_path("s1").unwrap().exists());

        drop(lease);
        mgr.delete("s1").unwrap();
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
