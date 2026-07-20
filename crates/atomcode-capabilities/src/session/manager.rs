//! `SessionManager` — the two-tier on-disk session store + its fast-listing metadata.
//!
//! Pure storage: no kernel coupling beyond serializing the kernel's `SessionSnapshot`.
//! The hooks (snapshot / transcript) and the recall tool call into this; the manager
//! itself does only file IO, so it is fully unit-testable with a temp root.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Barrier, Mutex};

use atomcode_kernel::message::SessionSnapshot;
use serde::{Deserialize, Serialize};

/// Fast-listing metadata for ONE session — read to populate a `/resume` picker WITHOUT
/// parsing the (large) snapshot / transcript files. Persisted as `<id>.meta`.
pub const META_VERSION: u32 = 1;

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
    /// The divider renders AFTER this many messages of the snapshot.
    pub after_message: usize,
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

    pub fn snapshot_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.snapshot"))
    }
    pub fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.meta"))
    }
    fn meta_lock_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.meta.lock"))
    }
    /// The append-only transcript path the [`TranscriptHook`](super::TranscriptHook)
    /// writes (and the recall tool reads).
    pub fn jsonl_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    /// Persist the working-set snapshot (atomic). Overwrites every turn.
    pub fn save_snapshot(&self, id: &str, snap: &SessionSnapshot) -> io::Result<()> {
        let bytes = serde_json::to_vec(snap).map_err(invalid_data)?;
        atomic_write(&self.snapshot_path(id), &bytes)
    }

    pub fn load_snapshot(&self, id: &str) -> io::Result<SessionSnapshot> {
        let bytes = fs::read(self.snapshot_path(id))?;
        serde_json::from_slice(&bytes).map_err(invalid_data)
    }

    pub fn write_meta(&self, meta: &SessionMeta) -> io::Result<()> {
        self.with_meta_lock(&meta.id, || self.write_meta_unlocked(meta))
    }

    fn write_meta_unlocked(&self, meta: &SessionMeta) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(meta).map_err(invalid_data)?;
        atomic_write(&self.meta_path(&meta.id), &bytes)
    }

    /// Atomically mutate an existing meta across threads and processes. The advisory
    /// lock covers the whole read-modify-write sequence; `write_meta` alone only
    /// serializes complete replacements and must not be used with a stale prior read.
    pub fn update_meta(&self, id: &str, update: impl FnOnce(&mut SessionMeta)) -> io::Result<()> {
        self.with_meta_lock(id, || {
            let mut meta = self.read_meta(id)?;
            update(&mut meta);
            self.write_meta_unlocked(&meta)
        })
    }

    pub(crate) fn update_meta_or_insert(
        &self,
        id: &str,
        new_meta: SessionMeta,
        update: impl FnOnce(&mut SessionMeta),
    ) -> io::Result<()> {
        self.with_meta_lock(id, || {
            let mut meta = match self.read_meta(id) {
                Ok(meta) => meta,
                Err(error) if error.kind() == io::ErrorKind::NotFound => new_meta,
                Err(error) => return Err(error),
            };
            update(&mut meta);
            self.write_meta_unlocked(&meta)
        })
    }

    fn with_meta_lock<T>(
        &self,
        id: &str,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        fs::create_dir_all(&self.root)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.meta_lock_path(id))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        operation()
    }

    pub fn read_meta(&self, id: &str) -> io::Result<SessionMeta> {
        let bytes = fs::read(self.meta_path(id))?;
        let meta: SessionMeta = serde_json::from_slice(&bytes).map_err(invalid_data)?;
        // The forward-compat seam, READER-ENFORCED (same rule as the kernel's
        // SNAPSHOT_VERSION check): a file from a future breaking schema may still
        // deserialize under this layout — refuse rather than silently misinterpret.
        if meta.v > META_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("meta schema v{} > supported v{META_VERSION}", meta.v),
            ));
        }
        #[cfg(test)]
        if let Some(pause) = self.meta_read_pause.lock().unwrap().take() {
            pause.entered.wait();
            pause.resume.wait();
        }
        Ok(meta)
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
            if let Ok(bytes) = fs::read(&path) {
                if let Ok(meta) = serde_json::from_slice::<SessionMeta>(&bytes) {
                    // Future-schema metas are skipped like malformed ones (reader-
                    // enforced version bound; see read_meta).
                    if meta.v <= META_VERSION {
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
    pub fn rename(&self, id: &str, name: &str) -> io::Result<()> {
        self.update_meta(id, |meta| {
            meta.name = name.to_string();
            meta.user_renamed = true;
        })
    }

    /// Remove a session's three files. A missing file is not an error (idempotent).
    pub fn delete(&self, id: &str) -> io::Result<()> {
        for p in [self.snapshot_path(id), self.meta_path(id), self.jsonl_path(id)] {
            match fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

fn invalid_data(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Write `bytes` to `path` atomically: write a sibling `*.tmp` then `rename` over the
/// target, so a crash mid-write never leaves a half-written (corrupt) session file. The
/// tmp's extension (`…tmp`) is ignored by [`SessionManager::list`]'s `*.meta` filter,
/// so a leftover tmp from a crash never appears as a session.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;

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

    #[test]
    fn meta_round_trips_and_list_is_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.write_meta(&SessionMeta::new("old", "/p", 1_000)).unwrap();
        mgr.write_meta(&SessionMeta::new("new", "/p", 2_000)).unwrap();

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
            mgr.meta_path("legacy"),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let meta = mgr.read_meta("legacy").unwrap();
        mgr.write_meta(&meta).unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&std::fs::read(mgr.meta_path("legacy")).unwrap()).unwrap();

        assert_eq!(rewritten["ai_named"], false);
        assert_eq!(rewritten["turn_stats"][0]["round_count"], 0);
        assert_eq!(rewritten["turn_stats"][0]["used_tokens"], 0);
        assert_eq!(rewritten["turn_stats"][0]["ctx_window"], 0);
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
            mgr.meta_path("named"),
            serde_json::to_vec_pretty(&native).unwrap(),
        )
        .unwrap();

        mgr.rename("named", "User title").unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&std::fs::read(mgr.meta_path("named")).unwrap()).unwrap();

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
        std::fs::write(mgr.jsonl_path("s1"), b"{}\n").unwrap();

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
    fn delete_removes_all_three_files_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_root(dir.path());
        mgr.write_meta(&SessionMeta::new("s1", "/p", 1)).unwrap();
        mgr.save_snapshot("s1", &snap(&["x"])).unwrap();
        std::fs::write(mgr.jsonl_path("s1"), b"{}\n").unwrap();

        mgr.delete("s1").unwrap();
        assert!(!mgr.meta_path("s1").exists());
        assert!(!mgr.snapshot_path("s1").exists());
        assert!(!mgr.jsonl_path("s1").exists());
        // Idempotent: deleting again is fine.
        mgr.delete("s1").unwrap();
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
        assert!(leftovers.is_empty(), "no .tmp must survive a successful write");
    }
}
