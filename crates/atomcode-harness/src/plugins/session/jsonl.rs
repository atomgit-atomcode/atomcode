use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use fs2::FileExt;
use serde::Serialize;
use serde_json::Value;

use crate::seams::SessionPersistence;
use crate::session::{LoggedEvent, SessionEvent, SessionHeader};

// Match the native store without enabling its session subsystem in the harness.
const MAX_JSONL_BYTES: u64 = 512 * 1024 * 1024;
// Deletion only needs to identify the first record, even in an over-limit log.
const OWNERSHIP_PROBE_BYTES: u64 = 16 * 1024 * 1024;

fn at(path: &Path, error: impl std::fmt::Display) -> String {
    format!("{}: {error}", path.display())
}

fn too_large(path: &Path) -> String {
    at(
        path,
        format_args!("session JSONL exceeds the {MAX_JSONL_BYTES}-byte limit (512 MiB)"),
    )
}

fn remove_if_present(path: &Path, result: io::Result<()>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(at(path, e)),
    }
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(at(path, e)),
    }
}

fn regular(path: &Path) -> Result<bool, String> {
    match metadata(path)? {
        None => Ok(false),
        Some(m) if m.is_file() => Ok(true),
        Some(_) => Err(at(path, "expected a regular file")),
    }
}

fn directory(path: &Path) -> Result<(), String> {
    if metadata(path)?.is_some_and(|m| !m.is_dir()) {
        return Err(at(path, "expected a directory"));
    }
    Ok(())
}

fn open_lock(path: &Path) -> Result<File, String> {
    regular(path)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| at(path, e))
}

struct Batch {
    encoded: Vec<u8>,
    remaining: u64,
    overflow: bool,
}

impl Write for Batch {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            self.overflow = true;
            return Err(io::Error::other("session JSONL limit exceeded"));
        }
        self.encoded.extend_from_slice(bytes);
        self.remaining -= bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize)]
struct EventRecord<'a> {
    seq: u64,
    at: u64,
    event: &'a SessionEvent,
}

fn file_len(path: &Path) -> Result<Option<u64>, String> {
    match metadata(path)? {
        None => Ok(None),
        Some(m) if m.is_file() => {
            if m.len() > MAX_JSONL_BYTES {
                return Err(too_large(path));
            }
            Ok(Some(m.len()))
        }
        Some(_) => Err(at(path, "expected a regular file")),
    }
}

// Call with the session lease held. Encode and check the entire batch before
// changing the file, and roll back a partial OS write before reporting it.
fn append_records<T: Serialize>(
    path: &Path,
    records: impl IntoIterator<Item = T>,
) -> Result<(), String> {
    let mut records = records.into_iter().peekable();
    if records.peek().is_none() {
        return Ok(());
    }
    let len = file_len(path)?;
    let mut existing = len
        .map(|_| OpenOptions::new().read(true).write(true).open(path))
        .transpose()
        .map_err(|e| at(path, e))?;
    let mut batch = Batch {
        encoded: Vec::new(),
        remaining: MAX_JSONL_BYTES - len.unwrap_or_default(),
        overflow: false,
    };
    if let (Some(file), Some(size)) = (&mut existing, len.filter(|size| *size > 0)) {
        let mut last = [0];
        file.seek(SeekFrom::Start(size - 1))
            .map_err(|e| at(path, e))?;
        file.read_exact(&mut last).map_err(|e| at(path, e))?;
        if last[0] != b'\n' {
            // A complete final record without a newline is valid on read.
            // Count its separator as part of this batch before writing it.
            batch.write_all(b"\n").map_err(|_| too_large(path))?;
        }
    }
    for record in records {
        let encoded = serde_json::to_writer(&mut batch, &record)
            .and_then(|()| batch.write_all(b"\n").map_err(serde_json::Error::io));
        if let Err(error) = encoded {
            return Err(if batch.overflow {
                too_large(path)
            } else {
                at(path, error)
            });
        }
    }
    let mut file = match existing {
        Some(file) => file,
        None => OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| at(path, e))?,
    };
    if file.seek(SeekFrom::End(0)).map_err(|e| at(path, e))? != len.unwrap_or_default() {
        return Err(at(path, "session JSONL changed before append"));
    }
    if let Err(error) = file.write_all(&batch.encoded) {
        let rollback = if let Some(len) = len {
            file.set_len(len)
        } else {
            drop(file);
            fs::remove_file(path)
        };
        return Err(match rollback {
            Ok(()) => at(path, error),
            Err(rollback) => at(
                path,
                format_args!("{error}; append rollback failed: {rollback}"),
            ),
        });
    }
    Ok(())
}

fn append_events(path: &Path, events: &[LoggedEvent]) -> Result<(), String> {
    append_records(
        path,
        events
            .iter()
            .filter(|e| !e.event.is_transient())
            .map(|e| EventRecord {
                seq: e.seq,
                at: e.at,
                event: &e.event,
            }),
    )
}

pub(super) struct LiveJournal {
    path: PathBuf,
    _lease: File,
    failed: AtomicBool,
}

impl LiveJournal {
    pub(super) fn commit(&self, event: &LoggedEvent) {
        if !self.failed.load(Ordering::Relaxed) {
            if let Err(error) = append_events(&self.path, std::slice::from_ref(event)) {
                self.failed.store(true, Ordering::Relaxed);
                eprintln!("session-persistence: {error}; subsequent events for this session will not be saved");
            }
        }
    }
}

pub(super) struct JsonlStore {
    root: PathBuf,
    /// The project bucket new sessions are written into.
    bucket: String,
}

impl JsonlStore {
    pub(super) fn new(root: PathBuf, bucket: String) -> Self {
        Self { root, bucket }
    }

    fn lock_dir(&self) -> Result<PathBuf, String> {
        // The native catalog already skips its reserved harness directory.
        // Keep our coordination files there even for old shared roots.
        let harness = self.root.join("harness");
        directory(&harness)?;
        let locks = harness.join(".jsonl-locks");
        directory(&locks)?;
        Ok(locks)
    }

    // The root lock protects lease creation/removal and the empty-bucket
    // check. A live writer only needs its per-session lease to append.
    fn lock_root(&self) -> Result<File, String> {
        let locks = self.lock_dir()?;
        fs::create_dir_all(&locks).map_err(|e| at(&locks, e))?;
        let path = locks.join("root.lock");
        let lock = open_lock(&path)?;
        FileExt::lock_exclusive(&lock).map_err(|e| at(&path, e))?;
        Ok(lock)
    }

    fn lease_path(&self, journal: &Path) -> Result<PathBuf, String> {
        let locks = self.lock_dir()?;
        let scope = if journal.parent() == Some(self.root.as_path()) {
            locks.join("legacy")
        } else {
            locks.join(&self.bucket)
        };
        directory(&scope)?;
        Ok(scope
            .join(journal.file_name().expect("journal file name"))
            .with_extension("lock"))
    }

    fn lease(&self, journal: &Path) -> Result<File, String> {
        let path = self.lease_path(journal)?;
        let parent = path.parent().expect("lease directory");
        fs::create_dir_all(parent).map_err(|e| at(parent, e))?;
        let lease = open_lock(&path)?;
        FileExt::try_lock_exclusive(&lease).map_err(|e| {
            if e.kind() == fs2::lock_contended_error().kind()
                && e.raw_os_error() == fs2::lock_contended_error().raw_os_error()
            {
                at(journal, "session is in use")
            } else {
                at(&path, e)
            }
        })?;
        Ok(lease)
    }

    fn resolve(&self, session_id: &str) -> Result<PathBuf, String> {
        directory(&self.dir())?;
        let path = self.path(session_id);
        let legacy = self.legacy_path(session_id);
        if regular(&path)? || !regular(&legacy)? {
            Ok(path)
        } else {
            Ok(legacy)
        }
    }

    fn resolve_write(&self, session_id: &str) -> Result<PathBuf, String> {
        let path = self.resolve(session_id)?;
        Self::check_native_namespace(&path)?;
        if file_len(&path)?.is_some_and(|size| size > 0) {
            Self::check_record_owner(&path, session_id)?;
        }
        Ok(path)
    }

    fn begin_at(path: &Path, header: &SessionHeader) -> Result<(), String> {
        if file_len(path)?.is_some() {
            return Ok(());
        }
        let parent = path.parent().expect("journal directory");
        fs::create_dir_all(parent).map_err(|e| at(parent, e))?;
        #[derive(Serialize)]
        struct HeaderRecord<'a> {
            header: &'a SessionHeader,
        }
        append_records(path, [HeaderRecord { header }])
    }

    pub(super) fn open_live(&self, header: &SessionHeader) -> Result<LiveJournal, String> {
        let _root = self.lock_root()?;
        let path = self.resolve_write(&header.id)?;
        let lease = self.lease(&path)?;
        Self::begin_at(&path, header)?;
        Ok(LiveJournal {
            path,
            _lease: lease,
            failed: AtomicBool::new(false),
        })
    }

    /// One path segment, whatever the id claims to be. Ids are minted here but a
    /// resumed one arrives from outside, and a crafted id must not escape.
    fn safe(session_id: &str) -> String {
        session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// The project's bucket directory.
    pub(crate) fn dir(&self) -> PathBuf {
        self.root.join(&self.bucket)
    }

    fn path(&self, session_id: &str) -> PathBuf {
        self.dir().join(format!("{}.jsonl", Self::safe(session_id)))
    }

    /// Where sessions written before bucketing existed still live.
    ///
    /// Kept in place so resuming one appends to the same log.
    fn legacy_path(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{}.jsonl", Self::safe(session_id)))
    }

    /// The first line, when it is a header. A file from before headers begins
    /// with an event instead, and reads the same as one whose header was lost:
    /// a session with no recorded identity beyond its file name.
    fn parse_header(path: &std::path::Path, text: &str) -> Result<Option<SessionHeader>, String> {
        let Some(first) = text.lines().find(|l| !l.trim().is_empty()) else {
            return Ok(None);
        };
        let value: Value =
            serde_json::from_str(first).map_err(|e| format!("{}:1: {e}", path.display()))?;
        let Some(header) = value.get("header") else {
            return Ok(None);
        };
        let header: SessionHeader = serde_json::from_value(header.clone())
            .map_err(|e| format!("{}:1: header: {e}", path.display()))?;
        if header.version > crate::session::SESSION_FORMAT_VERSION {
            return Err(format!(
                "{}: session format {} is newer than this build reads ({})",
                path.display(),
                header.version,
                crate::session::SESSION_FORMAT_VERSION
            ));
        }
        Ok(Some(header))
    }

    /// Every session file in this project's bucket, newest first. This
    /// project's, not every session on the machine: a list that crossed
    /// projects would make "what did we do here" unanswerable by the same
    /// amount the flat layout did.
    fn stored(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.dir()) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                (path.extension()? == "jsonl")
                    .then(|| path.file_stem()?.to_str().map(str::to_string))
                    .flatten()
            })
            .collect();
        ids.sort();
        ids.reverse();
        ids
    }

    pub(super) fn stored_header(&self, session_id: &str) -> Option<SessionHeader> {
        self.read_header(session_id).ok().flatten()
    }

    fn read_header(&self, session_id: &str) -> Result<Option<SessionHeader>, String> {
        let path = self.resolve(session_id)?;
        if !regular(&path)? {
            return Ok(None);
        }
        let mut reader = Self::reader(&path)?;
        let first = Self::first_record(&path, &mut reader)?;
        Self::parse_header(&path, &first)
    }

    fn reader(path: &Path) -> Result<BufReader<io::Take<File>>, String> {
        let file = File::open(path).map_err(|e| at(path, e))?;
        if file.metadata().map_err(|e| at(path, e))?.len() > MAX_JSONL_BYTES {
            return Err(too_large(path));
        }
        // The extra byte distinguishes EOF at the limit from growth after open.
        Ok(BufReader::new(file.take(MAX_JSONL_BYTES + 1)))
    }

    fn first_record(path: &Path, reader: &mut impl BufRead) -> Result<String, String> {
        let mut line = String::new();
        let mut bytes = 0;
        loop {
            let read = reader.read_line(&mut line).map_err(|e| at(path, e))?;
            bytes += read as u64;
            if bytes > MAX_JSONL_BYTES {
                return Err(too_large(path));
            }
            if read == 0 || !line.trim().is_empty() {
                return Ok(line);
            }
            line.clear();
        }
    }

    fn read_at(path: &Path) -> Result<Vec<LoggedEvent>, String> {
        let mut reader = Self::reader(path)?;
        let mut events = Vec::new();
        let mut line = String::new();
        let mut bytes = 0;
        let mut line_number = 0;
        loop {
            line.clear();
            let read = reader.read_line(&mut line).map_err(|e| at(path, e))?;
            if read == 0 {
                return Ok(events);
            }
            bytes += read as u64;
            if bytes > MAX_JSONL_BYTES {
                return Err(too_large(path));
            }
            line_number += 1;
            if line.trim().is_empty() {
                continue;
            }
            let error_at_line = |e| format!("{}:{line_number}: {e}", path.display());
            let value: Value = serde_json::from_str(&line).map_err(error_at_line)?;
            if value.get("header").is_some() && value.get("event").is_none() {
                continue;
            }
            let seq = value.get("seq").and_then(Value::as_u64).unwrap_or(0);
            let at = value.get("at").and_then(Value::as_u64).unwrap_or(0);
            let event = serde_json::from_value(value.get("event").cloned().unwrap_or(Value::Null))
                .map_err(error_at_line)?;
            events.push(LoggedEvent { seq, at, event });
        }
    }

    fn check_native_namespace(path: &Path) -> Result<(), String> {
        // Some installations put harness journals beside native sessions.
        // A native session can exist before it has a transcript.
        for ext in [
            "index",
            "events",
            "meta",
            "snapshot",
            "snapshot.inflight",
            "json",
        ] {
            if metadata(&path.with_extension(ext))?.is_some() {
                return Err(at(path, "session ID also belongs to the native store"));
            }
        }
        Ok(())
    }

    fn check_record_owner(path: &Path, session_id: &str) -> Result<(), String> {
        let file = File::open(path).map_err(|e| at(path, e))?;
        let mut reader = BufReader::new(file.take(OWNERSHIP_PROBE_BYTES));
        let first = Self::first_record(path, &mut reader)?;
        let record: Value = serde_json::from_str(&first).map_err(|e| {
            at(
                path,
                format_args!("cannot identify the harness journal: {e}"),
            )
        })?;
        let header_id = record.pointer("/header/id").and_then(Value::as_str);
        let owns = match header_id {
            Some(id) => {
                Self::safe(id) == Self::safe(session_id)
                    && (id == session_id
                        || path.file_stem().and_then(|s| s.to_str()) == Some(session_id))
            }
            None => {
                record.get("seq").and_then(Value::as_u64).is_some() && record.get("event").is_some()
            }
        };
        if !owns || record.get("turn_id").is_some() {
            return Err(at(path, "cannot identify the harness journal"));
        }
        Ok(())
    }

    fn delete_sync(&self, session_id: &str) -> Result<bool, String> {
        let _root = self.lock_root()?;
        let path = self.resolve(session_id)?;
        if !regular(&path)? {
            return Ok(false);
        }
        if path != self.legacy_path(session_id) && regular(&self.legacy_path(session_id))? {
            return Err(at(&path, "session also has a flat legacy journal"));
        }
        let lease = self.lease(&path)?;
        Self::check_native_namespace(&path)?;
        Self::check_record_owner(&path, session_id)?;

        // Leave the identifying journal until last, so retry can finish any
        // interrupted cleanup. Match only these exact, session-owned names.
        for ext in ["ui", "ui.json", "todos"] {
            let sidecar = path.with_extension(ext);
            remove_if_present(&sidecar, fs::remove_file(&sidecar))?;
        }
        for ext in ["artifacts", "display-images"] {
            let attachments = path.with_extension(ext);
            remove_if_present(&attachments, fs::remove_dir_all(&attachments))?;
        }
        // Every lease opener holds the root lock, so there can be no waiters
        // on the unlinked lease file. Keep holding the root lock through rmdir.
        drop(lease);
        let lease_path = self.lease_path(&path)?;
        remove_if_present(&lease_path, fs::remove_file(&lease_path))?;
        remove_if_present(&path, fs::remove_file(&path))?;
        if path.parent() == Some(self.dir().as_path()) {
            let bucket = self.dir();
            match fs::remove_dir(&bucket) {
                Ok(()) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) => {}
                Err(e) => return Err(at(&bucket, e)),
            }
        }
        Ok(true)
    }
}

#[async_trait]
impl SessionPersistence for JsonlStore {
    fn location(&self, session_id: &str) -> Option<String> {
        self.resolve(session_id)
            .ok()
            .map(|path| path.display().to_string())
    }
    async fn begin(&self, header: &SessionHeader) -> Result<(), String> {
        self.open_live(header).map(drop)
    }
    async fn header(&self, session_id: &str) -> Result<Option<SessionHeader>, String> {
        self.read_header(session_id)
    }

    async fn append(&self, session_id: &str, events: &[LoggedEvent]) -> Result<(), String> {
        if events.iter().all(|event| event.event.is_transient()) {
            return Ok(());
        }
        let _root = self.lock_root()?;
        let path = self.resolve_write(session_id)?;
        let _lease = self.lease(&path)?;
        let parent = path.parent().expect("journal directory");
        fs::create_dir_all(parent).map_err(|e| at(parent, e))?;
        append_events(&path, events)
    }

    async fn load(&self, session_id: &str) -> Result<Vec<LoggedEvent>, String> {
        let path = self.resolve(session_id)?;
        if !regular(&path)? {
            return Ok(Vec::new());
        }
        Self::read_at(&path)
    }

    async fn delete(&self, session_id: &str) -> Result<bool, String> {
        self.delete_sync(session_id)
    }

    async fn list(&self) -> Result<Vec<String>, String> {
        Ok(self
            .stored()
            .into_iter()
            .filter(|id| self.stored_header(id).is_none_or(|h| h.parent.is_none()))
            .collect())
    }

    async fn children(&self, parent: &str) -> Result<Vec<SessionHeader>, String> {
        Ok(self
            .stored()
            .into_iter()
            .filter_map(|id| self.stored_header(&id))
            .filter(|h| h.parent.as_deref() == Some(parent))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    struct TestStore(JsonlStore);

    impl TestStore {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "atomcode-jsonl-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            Self(JsonlStore::new(root, "bucket".into()))
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0.root);
        }
    }

    fn event(seq: u64) -> LoggedEvent {
        LoggedEvent {
            seq,
            at: 1,
            event: SessionEvent::TurnStart { turn: 1 },
        }
    }

    fn record_len(event: &LoggedEvent) -> u64 {
        serde_json::to_vec(&EventRecord {
            seq: event.seq,
            at: event.at,
            event: &event.event,
        })
        .unwrap()
        .len() as u64
            + 1
    }

    fn pad_journal(path: &Path, size: u64) {
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(size - 1)).unwrap();
        file.write_all(b"\n").unwrap();
    }

    #[tokio::test]
    async fn oversized_journal_rejects_reads_and_writes_but_can_be_deleted() {
        let test = TestStore::new();
        let store = &test.0;
        store.begin(&SessionHeader::new("large")).await.unwrap();
        let path = store.path("large");
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_JSONL_BYTES + 1)
            .unwrap();

        for error in [
            store.header("large").await.unwrap_err(),
            store.load("large").await.unwrap_err(),
            store.append("large", &[event(1)]).await.unwrap_err(),
        ] {
            assert!(
                error.contains("536870912") && error.contains("large.jsonl"),
                "{error}"
            );
        }
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_JSONL_BYTES + 1);
        assert!(store.delete("large").await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_batches_cannot_overrun_the_file_limit() {
        let test = TestStore::new();
        let store = &test.0;
        store.begin(&SessionHeader::new("race")).await.unwrap();
        let path = store.path("race");
        let len = record_len(&event(1));
        pad_journal(&path, MAX_JSONL_BYTES - len);
        let go = Arc::new(tokio::sync::Barrier::new(2));
        let write = |seq| {
            let peer = JsonlStore::new(store.root.clone(), store.bucket.clone());
            let go = go.clone();
            tokio::spawn(async move {
                go.wait().await;
                peer.append("race", &[event(seq)]).await
            })
        };
        let (first, second) = tokio::join!(write(1), write(2));
        let results = [first.unwrap(), second.unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(results
            .into_iter()
            .find_map(Result::err)
            .unwrap()
            .contains("536870912"));
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_JSONL_BYTES);
    }

    #[tokio::test]
    async fn rejected_batch_leaves_no_prefix_and_stops_live_commits() {
        let test = TestStore::new();
        let store = &test.0;
        let header = SessionHeader::new("live");
        let path = store.path("live");
        store.begin(&header).await.unwrap();
        let len = MAX_JSONL_BYTES - record_len(&event(1));
        pad_journal(&path, len);
        assert!(store.append("live", &[event(1), event(2)]).await.is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), len);

        let journal = store.open_live(&header).unwrap();
        journal.commit(&event(1000));
        journal.commit(&event(1));
        assert_eq!(fs::metadata(&path).unwrap().len(), len);
        assert!(store.delete("live").await.unwrap_err().contains("in use"));
        drop(journal);
        assert!(store.delete("live").await.unwrap());
    }

    #[tokio::test]
    async fn deletion_cleans_owned_files_and_leaves_other_sessions() {
        let test = TestStore::new();
        let store = &test.0;
        for id in ["delete", "keep"] {
            store.begin(&SessionHeader::new(id)).await.unwrap();
            store.append(id, &[event(1)]).await.unwrap();
        }
        let path = store.path("delete");
        fs::write(path.with_extension("ui.json"), "{}").unwrap();
        let artifacts = path.with_extension("artifacts");
        fs::create_dir(&artifacts).unwrap();
        fs::write(artifacts.join("output"), "owned").unwrap();

        assert!(store.delete("delete").await.unwrap());
        assert!(!store.delete("delete").await.unwrap());
        assert!(!artifacts.exists() && !path.with_extension("ui.json").exists());
        assert_eq!(store.list().await.unwrap(), ["keep"]);
        assert_eq!(store.load("keep").await.unwrap(), [event(1)]);
        assert!(store.delete("keep").await.unwrap());
        assert!(!store.dir().exists());
    }

    #[tokio::test]
    async fn legacy_log_keeps_its_location_and_native_data_is_not_deleted() {
        let test = TestStore::new();
        let store = &test.0;
        fs::create_dir_all(&store.root).unwrap();
        let legacy = store.legacy_path("legacy");
        fs::write(
            &legacy,
            b"\n{\"seq\":1,\"event\":{\"kind\":\"turn_start\",\"turn\":1}}",
        )
        .unwrap();
        store.append("legacy", &[event(2)]).await.unwrap();
        assert!(!store.path("legacy").exists());
        let events = store.load("legacy").await.unwrap();
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(store.delete("legacy").await.unwrap());

        let flat_native = store.legacy_path("flat-native");
        let native_record = r#"{"v":1,"turn_id":1}"#;
        fs::write(&flat_native, native_record).unwrap();
        assert!(store.append("flat-native", &[event(1)]).await.is_err());
        assert_eq!(fs::read_to_string(&flat_native).unwrap(), native_record);

        fs::create_dir(store.dir()).unwrap();
        let native = store.path("native");
        fs::write(&native, native_record).unwrap();
        assert!(store.append("native", &[event(1)]).await.is_err());
        assert_eq!(fs::read_to_string(&native).unwrap(), native_record);
        let pending_native = store.path("pending-native");
        fs::write(pending_native.with_extension("index"), "{}").unwrap();
        assert!(store
            .begin(&SessionHeader::new("pending-native"))
            .await
            .is_err());
        assert!(!pending_native.exists());
        let sidecar = native.with_extension("ui.json");
        fs::write(&sidecar, "native").unwrap();
        assert!(store
            .delete("native")
            .await
            .unwrap_err()
            .contains("identify"));
        assert!(native.exists() && sidecar.exists());
    }
}
