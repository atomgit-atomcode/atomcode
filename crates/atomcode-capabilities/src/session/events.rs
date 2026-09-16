//! Sessions whose content is their event log (`docs/adr/0024`).
//!
//! The store keeps everything it had — project buckets, the catalog, leases,
//! rename, delete, fork, the metadata index, ownership checks — and swaps what a
//! session *is* from a snapshot rewritten every turn to the harness's facts,
//! appended as they are committed. A snapshot is still what several readers
//! want, so reading one projects it from the log.
//!
//! Every file of an event session is named so that a build from before this
//! format finds nothing it recognises (`docs/adr/0024` §16): the log is
//! `<id>.events`, the metadata `<id>.index`, and the sidecars drop the `.json`
//! and `.jsonl` suffixes an older scan claims. Rolling back to such a build
//! loses sight of these sessions; it never reads half of one and writes a
//! diverging history over it.
//!
//! `<id>.events` is JSON lines: a `{"header": …}` line written when the session
//! is created, then one `{"seq", "at", "event"}` record per committed fact.
//! Streamed chunks are never written — the complete message follows them
//! (`docs/adr/0024` §7) — so the numbering on disk has gaps.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use atomcode_kernel::message::SessionSnapshot;
use atomcode_kernel::session::{
    derive_messages_with_meta, LoggedEvent, SessionEvent, SessionHeader, SESSION_FORMAT_VERSION,
};

use super::manager::{
    io_at, open_append_file, read_regular_file_bounded, retry_transient_file_access, SessionLease,
    SessionManager, SessionMeta, SessionResult, SessionStoreError, StorageOwner, MAX_JSONL_BYTES,
    MAX_JSONL_LINE_BYTES,
};

/// What an event log line holds.
#[derive(serde::Deserialize)]
struct Line {
    #[serde(default)]
    header: Option<SessionHeader>,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    at: u64,
    #[serde(default)]
    event: Option<SessionEvent>,
}

impl SessionManager {
    /// Whether `id` is stored as an event log. The index is the commit point of
    /// such a session, so it is what decides.
    pub fn is_event_session(&self, id: &str) -> bool {
        self.index_path(id)
            .ok()
            .is_some_and(|path| fs::symlink_metadata(path).is_ok())
    }

    /// `<id>.events`.
    pub fn events_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "events")
    }

    /// `<id>.index`: an event session's metadata.
    pub fn index_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "index")
    }

    /// Create an event session under its lease: the log with its header first,
    /// then the index, which is when the catalog can see it. A session that
    /// already has any content is refused rather than overwritten.
    pub fn create_event_session(
        &self,
        lease: &SessionLease,
        header: &SessionHeader,
        meta: &SessionMeta,
    ) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        let id = lease.id();
        if header.id != id || meta.id != id {
            return Err(SessionStoreError::Corrupt {
                kind: "session events",
                message: format!(
                    "creating {id:?} with header {:?} and meta {:?}",
                    header.id, meta.id
                ),
            });
        }
        if meta.owner != StorageOwner::Native {
            return Err(SessionStoreError::OwnershipConflict {
                id: id.to_string(),
                owner: meta.owner.clone(),
                operation: "create event session",
            });
        }
        let header_line = header_line(header)?;
        self.with_meta_lock(id, || {
            for existing in [
                self.index_path(id)?,
                self.events_path(id)?,
                self.path_for(id, "meta")?,
                self.path_for(id, "snapshot")?,
            ] {
                if fs::symlink_metadata(&existing).is_ok() {
                    return Err(SessionStoreError::Corrupt {
                        kind: "session events",
                        message: format!("{} already exists", existing.display()),
                    });
                }
            }
            super::manager::atomic_write(&self.events_path(id)?, &header_line)?;
            self.write_index_unlocked(meta)
        })
    }

    /// Append committed facts under the session's lease. Chunks are skipped.
    ///
    /// One write, under the file's exclusive lock, so records from two writers
    /// can never interleave; the lease is what keeps there being one writer.
    pub fn append_events(&self, lease: &SessionLease, events: &[LoggedEvent]) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        let id = lease.id();
        if !self.is_event_session(id) {
            return Err(SessionStoreError::NotFound {
                path: self.index_path(id)?,
            });
        }
        let mut buffer = Vec::new();
        for logged in events {
            if matches!(logged.event, SessionEvent::AssistantChunk { .. }) {
                continue;
            }
            let line = serde_json::to_vec(&serde_json::json!({
                "seq": logged.seq,
                "at": logged.at,
                "event": logged.event,
            }))
            .map_err(|error| SessionStoreError::Corrupt {
                kind: "session event",
                message: error.to_string(),
            })?;
            if line.len() + 1 > MAX_JSONL_LINE_BYTES {
                return Err(SessionStoreError::TooLarge {
                    kind: "session event",
                    limit: MAX_JSONL_LINE_BYTES,
                    actual: line.len() + 1,
                });
            }
            buffer.extend_from_slice(&line);
            buffer.push(b'\n');
        }
        if buffer.is_empty() {
            return Ok(());
        }
        let path = self.events_path(id)?;
        let mut file = retry_transient_file_access(|| open_append_file(&path))?;
        retry_transient_file_access(|| {
            fs2::FileExt::lock_exclusive(&file).map_err(|e| io_at(&path, e))
        })?;
        let current = usize::try_from(file.metadata().map_err(|e| io_at(&path, e))?.len())
            .unwrap_or(usize::MAX);
        let next = current.saturating_add(buffer.len());
        if next > MAX_JSONL_BYTES {
            return Err(SessionStoreError::TooLarge {
                kind: "session events",
                limit: MAX_JSONL_BYTES,
                actual: next,
            });
        }
        file.write_all(&buffer).map_err(|e| io_at(&path, e))
    }

    /// The header an event session was created with.
    pub fn read_event_header(&self, id: &str) -> SessionResult<SessionHeader> {
        let (header, _) = self.read_event_log(id, true)?;
        header.ok_or_else(|| SessionStoreError::Corrupt {
            kind: "session events",
            message: format!("{id}: no header"),
        })
    }

    /// Every stored fact of an event session, in file order.
    pub fn load_events(&self, id: &str) -> SessionResult<Vec<LoggedEvent>> {
        Ok(self.read_event_log(id, false)?.1)
    }

    /// The conversation an event session's log projects to, as a snapshot.
    ///
    /// Messages only: the system prompt is assembled per request and was never
    /// a fact.
    pub fn project_snapshot(&self, id: &str) -> SessionResult<SessionSnapshot> {
        Ok(snapshot_of(&self.load_events(id)?))
    }

    fn read_event_log(
        &self,
        id: &str,
        header_only: bool,
    ) -> SessionResult<(Option<SessionHeader>, Vec<LoggedEvent>)> {
        let path = self.events_path(id)?;
        let bytes = read_regular_file_bounded(&path, "session events", MAX_JSONL_BYTES)?;
        let mut header = None;
        let mut events = Vec::new();
        for (index, raw) in bytes.split(|b| *b == b'\n').enumerate() {
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let line: Line =
                serde_json::from_slice(raw).map_err(|error| SessionStoreError::Corrupt {
                    kind: "session events",
                    message: format!("{}:{}: {error}", path.display(), index + 1),
                })?;
            if let Some(found) = line.header {
                if found.version > SESSION_FORMAT_VERSION {
                    return Err(SessionStoreError::FutureSchema {
                        kind: "session events",
                        found: found.version,
                        supported: SESSION_FORMAT_VERSION,
                    });
                }
                header = Some(found);
                if header_only {
                    break;
                }
                continue;
            }
            match (line.seq, line.event) {
                (Some(seq), Some(event)) => events.push(LoggedEvent {
                    seq,
                    at: line.at,
                    event,
                }),
                _ => {
                    return Err(SessionStoreError::Corrupt {
                        kind: "session events",
                        message: format!("{}:{}: not a record", path.display(), index + 1),
                    })
                }
            }
        }
        Ok((header, events))
    }

    fn write_index_unlocked(&self, meta: &SessionMeta) -> SessionResult<()> {
        super::manager::validate_meta(meta)?;
        let bytes = super::manager::serialize_pretty_bounded(
            meta,
            "session index",
            super::manager::MAX_META_BYTES,
        )?;
        super::manager::atomic_write(&self.index_path(&meta.id)?, &bytes)
    }

    /// Fork an event session: the new one starts from every fact the source has,
    /// with the source as its parent and all of them counted as inherited.
    pub(crate) fn fork_event_session(
        &self,
        source_id: &str,
        destination: &SessionLease,
        meta: &SessionMeta,
        now_ms: i64,
    ) -> SessionResult<()> {
        let source_header = self.read_event_header(source_id)?;
        let events = self.load_events(source_id)?;
        let mut header = source_header;
        header.id = destination.id().to_string();
        header.parent = Some(source_id.to_string());
        header.inherited = events.len();
        header.created_at = u64::try_from(now_ms).unwrap_or(0);
        self.create_event_session(destination, &header, meta)?;
        self.append_events(destination, &events)
    }

    /// Turn timestamps of an event session: when each turn started and ended.
    pub(crate) fn event_turn_timestamps(
        &self,
        id: &str,
    ) -> SessionResult<std::collections::BTreeMap<u64, super::transcript::TurnTimestamp>> {
        let mut out: std::collections::BTreeMap<u64, super::transcript::TurnTimestamp> =
            std::collections::BTreeMap::new();
        for logged in self.load_events(id)? {
            let at = i64::try_from(logged.at).unwrap_or(i64::MAX);
            match logged.event {
                SessionEvent::TurnStart { turn } => {
                    out.entry(turn)
                        .or_insert(super::transcript::TurnTimestamp {
                            started_at: None,
                            completed_at: at,
                        })
                        .started_at = Some(at);
                }
                SessionEvent::TurnEnd { turn, .. } => {
                    out.entry(turn)
                        .or_insert(super::transcript::TurnTimestamp {
                            started_at: None,
                            completed_at: at,
                        })
                        .completed_at = at;
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

/// A snapshot of what `events` project to.
pub fn snapshot_of(events: &[LoggedEvent]) -> SessionSnapshot {
    let mut snapshot = SessionSnapshot::new(derive_messages_with_meta(events));
    let turns = events.iter().map(|e| e.event.turn()).max().unwrap_or(0);
    snapshot.turn_counter = snapshot.turn_counter.max(turns);
    snapshot
}

fn header_line(header: &SessionHeader) -> SessionResult<Vec<u8>> {
    let mut line =
        serde_json::to_vec(&serde_json::json!({ "header": header })).map_err(|error| {
            SessionStoreError::Corrupt {
                kind: "session header",
                message: error.to_string(),
            }
        })?;
    line.push(b'\n');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::StorageOwner;
    use atomcode_kernel::event::StopReason;

    const BUCKET: &str = "0123456789abcdef";

    fn store() -> (tempfile::TempDir, SessionManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path().join(BUCKET));
        (dir, manager)
    }

    fn meta(id: &str) -> SessionMeta {
        let mut meta = SessionMeta::new(id, "/work", 1_000);
        meta.owner = StorageOwner::Native;
        meta
    }

    fn created(manager: &SessionManager, id: &str) -> SessionLease {
        let lease = manager.acquire_lease(id).unwrap();
        manager
            .create_event_session(&lease, &SessionHeader::new(id), &meta(id))
            .unwrap();
        lease
    }

    fn logged(seq: u64, at: u64, event: SessionEvent) -> LoggedEvent {
        LoggedEvent { seq, at, event }
    }

    fn a_turn() -> Vec<LoggedEvent> {
        vec![
            logged(1, 10, SessionEvent::TurnStart { turn: 1 }),
            logged(
                2,
                11,
                SessionEvent::UserMessage {
                    turn: 1,
                    text: "hello".into(),
                    images: vec![],
                },
            ),
            logged(
                3,
                12,
                SessionEvent::AssistantChunk {
                    turn: 1,
                    round: 1,
                    delta: "hi".into(),
                    reasoning: false,
                },
            ),
            logged(
                4,
                13,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    round: 1,
                    text: "hi there".into(),
                    reasoning: String::new(),
                    tool_calls: vec![],
                    reasoning_blocks: Vec::new(),
                    meta: None,
                },
            ),
            logged(
                5,
                14,
                SessionEvent::TurnEnd {
                    turn: 1,
                    stop: StopReason::Stopped,
                    error: None,
                },
            ),
        ]
    }

    /// An event session keeps its facts — not the chunks — with their numbers
    /// and times; a snapshot read of it is the projection of those facts; its
    /// turn times come from them; and the catalog lists it.
    #[test]
    fn an_event_session_is_created_appended_and_read_back() {
        let (dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();

        let stored = manager.load_events("s1").unwrap();
        assert_eq!(
            stored.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 4, 5],
            "chunks are not written"
        );
        assert_eq!(
            stored.iter().map(|e| e.at).collect::<Vec<_>>(),
            vec![10, 11, 13, 14]
        );
        assert_eq!(manager.read_event_header("s1").unwrap().id, "s1");

        let loaded = manager.load_native_session("s1").unwrap();
        assert_eq!(
            loaded.snapshot.messages,
            derive_messages_with_meta(&stored),
            "a snapshot of an event session is its projection"
        );
        assert_eq!(loaded.meta.id, "s1");

        let times = manager.load_transcript_timestamps("s1").unwrap();
        assert_eq!(times[&1].started_at, Some(10));
        assert_eq!(times[&1].completed_at, 14);

        let scan = SessionManager::scan_catalog(dir.path());
        assert!(
            scan.entries.iter().any(|entry| entry.id == "s1"),
            "{scan:#?}"
        );
        assert!(scan.diagnostics.is_empty(), "{:#?}", scan.diagnostics);
    }

    /// Nothing an event session writes has a name a build from before the
    /// format scans for (`docs/adr/0024` §16): rolling back cannot pick up half
    /// of it.
    #[test]
    fn an_event_session_leaves_nothing_an_older_version_recognises() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        manager.rename("s1", "renamed").unwrap();
        manager.write_todo_sidecar("s1", &[], 2).unwrap();
        manager
            .commit_native_runtime_mutation(
                &lease,
                &SessionSnapshot::new(Vec::new()),
                |_, meta, _| {
                    meta.turn_count += 1;
                    Ok(())
                },
            )
            .unwrap();

        let names: Vec<String> = fs::read_dir(manager.root())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        for name in &names {
            for older in [".snapshot", ".meta", ".jsonl", ".json"] {
                assert!(
                    !name.ends_with(older),
                    "`{name}` is a file an older build would read: {names:?}"
                );
            }
        }
        assert!(names.iter().any(|n| n == "s1.events") && names.iter().any(|n| n == "s1.index"));
    }

    /// The log is the content: a runtime mutation writes only what sits beside
    /// it, whatever snapshot it is handed.
    #[test]
    fn a_runtime_mutation_leaves_the_log_alone() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        let before = fs::read(manager.events_path("s1").unwrap()).unwrap();

        manager
            .commit_native_runtime_mutation(
                &lease,
                &SessionSnapshot::new(Vec::new()),
                |current, meta, _| {
                    assert_eq!(current.messages.len(), 2, "handed the projection");
                    meta.turn_count = 7;
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(
            fs::read(manager.events_path("s1").unwrap()).unwrap(),
            before
        );
        assert_eq!(manager.read_meta("s1").unwrap().turn_count, 7);
        assert_eq!(manager.load_snapshot("s1").unwrap().messages.len(), 2);
    }

    /// Only the holder of the session's own lease appends, and only to an event
    /// session.
    #[test]
    fn appending_needs_the_sessions_own_lease() {
        let (_dir, manager) = store();
        let _lease = created(&manager, "s1");
        let other = created(&manager, "s2");
        let foreign = SessionManager::with_root(manager.root().join("elsewhere"));
        let stray = foreign.acquire_lease("s1").unwrap();
        assert!(matches!(
            manager.append_events(&stray, &a_turn()),
            Err(SessionStoreError::LeaseMismatch { .. })
        ));
        let snapshot_session = manager.acquire_lease("s3").unwrap();
        assert!(matches!(
            manager.append_events(&snapshot_session, &a_turn()),
            Err(SessionStoreError::NotFound { .. })
        ));
        manager.append_events(&other, &a_turn()).unwrap();
        assert!(manager.load_events("s1").unwrap().is_empty());
    }

    /// A log written by a newer build is refused, not misread.
    #[test]
    fn a_log_from_a_newer_format_is_refused() {
        let (_dir, manager) = store();
        let lease = manager.acquire_lease("s1").unwrap();
        let mut header = SessionHeader::new("s1");
        header.version = SESSION_FORMAT_VERSION + 1;
        manager
            .create_event_session(&lease, &header, &meta("s1"))
            .unwrap();
        assert!(matches!(
            manager.load_events("s1"),
            Err(SessionStoreError::FutureSchema { .. })
        ));
    }

    #[test]
    fn deleting_an_event_session_removes_its_files() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        manager.write_todo_sidecar("s1", &[], 2).unwrap();
        manager.delete(&lease).unwrap();
        let left: Vec<String> = fs::read_dir(manager.root())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".lease") && !name.ends_with(".lock"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert!(!manager.is_event_session("s1"));
    }

    /// A fork starts from every fact of the source, under its own identity, with
    /// the source as its parent and all of them counted as inherited.
    #[test]
    fn forking_an_event_session_copies_its_log() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();

        let (forked, _fork_lease) = manager.fork_native_session("s1", "s2", 2_000).unwrap();
        assert_eq!(forked.meta.id, "s2");
        assert_eq!(
            forked.meta.fork_info.as_ref().map(|f| f.parent_id.as_str()),
            Some("s1")
        );
        assert!(manager.is_event_session("s2"));
        assert_eq!(
            manager.load_events("s2").unwrap(),
            manager.load_events("s1").unwrap()
        );
        let header = manager.read_event_header("s2").unwrap();
        assert_eq!(header.parent.as_deref(), Some("s1"));
        assert_eq!(header.inherited, 4);
    }
}
