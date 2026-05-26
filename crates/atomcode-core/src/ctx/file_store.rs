//! In-memory file content store — D3 step 1.
//!
//! Why this exists: read_file's content used to live in conversation
//! tool_result messages. Each turn the LLM saw it again at full token
//! cost; compaction stripped it; the model then re-read the same file
//! and burned another full-content roundtrip. Across a 120-turn
//! atomgr session that pattern produced 47 reads on 10 unique files
//! plus three compactions destroying file content (datalog
//! 2026-05-06_10-22-35).
//!
//! `FileStore` decouples file content from conversation history. The
//! model's `read_file` ToolResult carries a tiny pointer (`store_id`
//! + preview); the actual bytes live here. `peek_file` is a separate
//! tool that fetches regions from this store with zero disk hits.
//! Compaction touches only pointers; content survives.
//!
//! Lifecycle: process-local (no persistence yet — that's D3b). Path
//! invalidation fires on `edit_file` / `write_file` success so a
//! stale `store_id` cannot serve outdated bytes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

/// One captured file snapshot.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub content: String,
    /// File mtime at insert time. `peek` validates against this — if the
    /// disk file changed (or was edited via our own write tools), the
    /// entry is stale and the caller is asked to re-read.
    pub mtime: SystemTime,
    pub size_bytes: usize,
    pub line_count: usize,
}

/// Process-local file content store.
///
/// `Default` constructs an empty store. Wrap in `Arc<RwLock<>>` for the
/// shared `ToolContext.file_store` field; the lock is taken briefly per
/// call (insert is one allocation + hash, peek is a HashMap lookup +
/// substring slice).
#[derive(Debug, Default)]
pub struct FileStore {
    entries: HashMap<String, FileEntry>,
    /// path → most recent store_id, so callers that only know the path
    /// (e.g. invalidate) can find what to drop. A path can have at
    /// most one live entry; subsequent reads of the same path reuse
    /// the slot.
    by_path: HashMap<PathBuf, String>,
}

impl FileStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a file snapshot into the store. Returns the assigned
    /// `store_id`. Any prior entry for the same `path` is replaced —
    /// re-reading a file overwrites its slot rather than accumulating
    /// stale copies.
    ///
    /// `store_id` shape: `fs_<8-hex-of-content-hash>`. Hash carries
    /// content+path so unrelated files can't collide; the prefix
    /// disambiguates from other id namespaces in logs.
    pub fn insert(&mut self, path: PathBuf, content: String, mtime: SystemTime) -> String {
        let store_id = derive_id(&path, &content);
        let line_count = content.lines().count();
        let size_bytes = content.len();
        let entry = FileEntry {
            path: path.clone(),
            content,
            mtime,
            size_bytes,
            line_count,
        };
        // Drop any prior entry for this path before reinserting.
        if let Some(old_id) = self.by_path.insert(path, store_id.clone()) {
            if old_id != store_id {
                self.entries.remove(&old_id);
            }
        }
        self.entries.insert(store_id.clone(), entry);
        store_id
    }

    /// Look up an entry by store_id. Returns `None` if invalidated or
    /// never inserted.
    pub fn get(&self, store_id: &str) -> Option<&FileEntry> {
        self.entries.get(store_id)
    }

    /// Look up the live store_id for a path (if any). Used by
    /// invalidate-on-edit and by `read_file` to detect "we already
    /// have this; reuse".
    pub fn store_id_for_path(&self, path: &std::path::Path) -> Option<&str> {
        self.by_path.get(path).map(String::as_str)
    }

    /// Compare the entry's recorded mtime to a freshly-stat'd one.
    /// Returns true when the disk has moved on and the entry should
    /// not serve. Caller (typically peek_file) returns a recovery
    /// hint pointing at re-read.
    pub fn is_stale(&self, store_id: &str, current_mtime: SystemTime) -> bool {
        match self.entries.get(store_id) {
            Some(e) => e.mtime != current_mtime,
            // Unknown id behaves as "stale" so callers route through the
            // same recovery path uniformly.
            None => true,
        }
    }

    /// Drop the entry (if any) for a path. Called by edit_file /
    /// write_file on success. No-op when the path was never in the
    /// store. Idempotent — calling twice with the same path is fine.
    pub fn invalidate(&mut self, path: &std::path::Path) {
        if let Some(store_id) = self.by_path.remove(path) {
            self.entries.remove(&store_id);
        }
    }

    /// Extract a 1-indexed inclusive line range. `[1, 1]` returns the
    /// first line; out-of-range tails are clamped. Returns `None`
    /// only if the store_id is unknown — empty regions return `Some("")`
    /// so callers can distinguish "no such entry" from "valid request,
    /// nothing in that range".
    pub fn peek_lines(&self, store_id: &str, start: usize, end: usize) -> Option<String> {
        let entry = self.entries.get(store_id)?;
        if start == 0 || start > entry.line_count {
            return Some(String::new());
        }
        let s = start.saturating_sub(1);
        let e = end.min(entry.line_count);
        if e < start {
            return Some(String::new());
        }
        let lines: Vec<&str> = entry.content.lines().collect();
        Some(lines[s..e].join("\n"))
    }

    /// Number of live entries — used by tests and the `/context`
    /// rich snapshot.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Derive a stable id from path + content. Same content at the same
/// path produces the same id — useful for de-duping repeated reads
/// of an unchanged file. Different content (post-edit) produces a
/// new id even if the path is the same; that's the lever we use to
/// detect "model is operating on a stale snapshot".
fn derive_id(path: &std::path::Path, content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    content.hash(&mut h);
    format!("fs_{:08x}", h.finish() & 0xFFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn three_lines() -> String {
        "alpha\nbeta\ngamma\n".to_string()
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut s = FileStore::new();
        let id = s.insert(PathBuf::from("/x.rs"), three_lines(), t(100));
        let e = s.get(&id).unwrap();
        assert_eq!(e.line_count, 3);
        assert_eq!(e.content, "alpha\nbeta\ngamma\n");
        assert_eq!(e.mtime, t(100));
    }

    #[test]
    fn store_id_lookup_by_path_returns_latest() {
        let mut s = FileStore::new();
        let id1 = s.insert(PathBuf::from("/x.rs"), "v1".into(), t(100));
        let id2 = s.insert(PathBuf::from("/x.rs"), "v2".into(), t(200));
        assert_ne!(id1, id2);
        // Old id displaced by new — only the latest survives.
        assert!(s.get(&id1).is_none());
        assert!(s.get(&id2).is_some());
        assert_eq!(
            s.store_id_for_path(std::path::Path::new("/x.rs")),
            Some(id2.as_str())
        );
    }

    #[test]
    fn peek_lines_extracts_inclusive_range() {
        let mut s = FileStore::new();
        let id = s.insert(PathBuf::from("/x.rs"), three_lines(), t(0));
        assert_eq!(s.peek_lines(&id, 1, 1).unwrap(), "alpha");
        assert_eq!(s.peek_lines(&id, 1, 2).unwrap(), "alpha\nbeta");
        assert_eq!(s.peek_lines(&id, 2, 3).unwrap(), "beta\ngamma");
        assert_eq!(s.peek_lines(&id, 1, 99).unwrap(), "alpha\nbeta\ngamma");
    }

    #[test]
    fn peek_lines_handles_zero_and_oob() {
        let mut s = FileStore::new();
        let id = s.insert(PathBuf::from("/x.rs"), three_lines(), t(0));
        // 0 / past-end / inverted ranges all yield Some("") — callers
        // can format a friendly "out of range" without an extra branch.
        assert_eq!(s.peek_lines(&id, 0, 1).unwrap(), "");
        assert_eq!(s.peek_lines(&id, 50, 99).unwrap(), "");
        assert_eq!(s.peek_lines(&id, 5, 2).unwrap(), "");
    }

    #[test]
    fn peek_lines_unknown_id_returns_none() {
        let s = FileStore::new();
        assert!(s.peek_lines("fs_00000000", 1, 1).is_none());
    }

    #[test]
    fn is_stale_detects_mtime_change() {
        let mut s = FileStore::new();
        let id = s.insert(PathBuf::from("/x.rs"), "x".into(), t(100));
        assert!(!s.is_stale(&id, t(100)));
        assert!(s.is_stale(&id, t(101)));
    }

    #[test]
    fn is_stale_unknown_id_treated_as_stale() {
        let s = FileStore::new();
        // Unknown id routes through the same "stale" path so callers
        // don't need separate branches.
        assert!(s.is_stale("fs_deadbeef", t(0)));
    }

    #[test]
    fn invalidate_drops_entry_for_path() {
        let mut s = FileStore::new();
        let id = s.insert(PathBuf::from("/x.rs"), "x".into(), t(100));
        assert!(s.get(&id).is_some());
        s.invalidate(std::path::Path::new("/x.rs"));
        assert!(s.get(&id).is_none());
        assert!(s.store_id_for_path(std::path::Path::new("/x.rs")).is_none());
    }

    #[test]
    fn invalidate_unknown_path_is_noop() {
        let mut s = FileStore::new();
        s.invalidate(std::path::Path::new("/nonexistent")); // no panic
        assert!(s.is_empty());
    }

    #[test]
    fn invalidate_only_affects_named_path() {
        let mut s = FileStore::new();
        let id_a = s.insert(PathBuf::from("/a.rs"), "a".into(), t(0));
        let id_b = s.insert(PathBuf::from("/b.rs"), "b".into(), t(0));
        s.invalidate(std::path::Path::new("/a.rs"));
        assert!(s.get(&id_a).is_none());
        assert!(s.get(&id_b).is_some());
    }

    #[test]
    fn derive_id_stable_for_same_input() {
        let p = std::path::Path::new("/x.rs");
        let id1 = derive_id(p, "hello");
        let id2 = derive_id(p, "hello");
        assert_eq!(id1, id2);
    }

    #[test]
    fn derive_id_changes_with_content() {
        let p = std::path::Path::new("/x.rs");
        assert_ne!(derive_id(p, "hello"), derive_id(p, "world"));
    }

    #[test]
    fn derive_id_changes_with_path() {
        assert_ne!(
            derive_id(std::path::Path::new("/a"), "x"),
            derive_id(std::path::Path::new("/b"), "x"),
        );
    }
}
