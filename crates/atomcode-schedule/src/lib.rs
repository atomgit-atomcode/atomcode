//! Shared host-owned automation definitions and run facts.
//!
//! Receives its storage directory from the host. Conversation content remains
//! owned by the native session log; this ledger only links to those sessions.
#[cfg(feature = "model-tools")]
mod model;
pub mod os;
pub mod service;
pub mod timing;

use anyhow::{bail, Context, Result};
use atomcode_config::schedule::ScheduleTask;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Definition {
    pub task: ScheduleTask,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEvent {
    Started {
        run_id: String,
        task_id: String,
        revision: String,
        prompt: String,
        cwd: String,
        at: i64,
    },
    Session {
        run_id: String,
        session_id: String,
        at: i64,
    },
    Finished {
        run_id: String,
        status: String,
        error: Option<String>,
        at: i64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryPage {
    pub events: Vec<RunEvent>,
    pub next_cursor: Option<usize>,
}

fn valid_id(id: &str) -> bool {
    if id.is_empty()
        || id.len() > 128
        || id.contains("..")
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return false;
    }
    let base = id.split('.').next().unwrap_or("").to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&base.as_str()) {
        return false;
    }
    if base.len() == 4
        && (base.starts_with("COM") || base.starts_with("LPT"))
        && matches!(base.as_bytes()[3], b'1'..=b'9')
    {
        return false;
    }
    true
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Read one committed JSONL record backwards without scanning older history.
fn line_before(file: &mut File, end: u64) -> Result<Option<(Vec<u8>, u64)>> {
    if end == 0 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(end - 1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    if last[0] != b'\n' {
        bail!("invalid history cursor or incomplete journal; refresh or repair run history");
    }
    let mut cursor = end - 1;
    let mut chunks = Vec::new();
    let mut total = 0;
    loop {
        let start = cursor.saturating_sub(4096);
        let mut bytes = vec![0; usize::try_from(cursor - start)?];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut bytes)?;
        let boundary = bytes.iter().rposition(|b| *b == b'\n');
        let begin = boundary.map(|p| p + 1).unwrap_or(0);
        let slice = bytes[begin..].to_vec();
        total += slice.len();
        if total > 1024 * 1024 {
            bail!("run metadata event exceeds one MiB");
        }
        chunks.push(slice);
        if boundary.is_some() || start == 0 {
            let position = start + begin as u64;
            let result = chunks.into_iter().rev().flatten().collect();
            return Ok(Some((result, position)));
        }
        cursor = start;
    }
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn path(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            bail!("invalid task id");
        }
        Ok(self.root.join(format!("{id}.json")))
    }
    fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.root)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.root.join(".catalog.lock"))?;
        file.lock_exclusive()?;
        Ok(file)
    }
    fn operation_lock(&self) -> Result<File> {
        fs::create_dir_all(&self.root)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.root.join(".operations.lock"))?;
        file.lock_exclusive()?;
        Ok(file)
    }
    fn registration_error(&self, id: &str) -> Result<Option<String>> {
        let path = self.run_dir(id)?.join("registration.json");
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn record_registration(&self, id: &str, error: &Option<String>) -> Result<()> {
        let dir = self.run_dir(id)?;
        let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
        temp.write_all(&serde_json::to_vec(error)?)?;
        temp.as_file().sync_all()?;
        temp.persist(dir.join("registration.json"))
            .map_err(|e| e.error)?;
        Ok(())
    }
    pub fn running(&self, id: &str) -> Result<bool> {
        self.path(id)?;
        let path = self.root.join("runs").join(id).join("lease");
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        match file.try_lock_exclusive() {
            Ok(()) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            Err(e) => Err(e.into()),
        }
    }
    pub fn get(&self, id: &str) -> Result<Definition> {
        let bytes = fs::read(self.path(id)?).context("cannot read scheduled task")?;
        let task: ScheduleTask =
            serde_json::from_slice(&bytes).context("invalid scheduled task data")?;
        if task.id != id {
            bail!("task identity does not match its filename");
        }
        Ok(Definition {
            task,
            revision: revision(&bytes),
        })
    }
    /// Cross-process change detector for browser subscriptions. This does not
    /// invoke an OS command or load conversation data.
    pub fn change_stamp(&self) -> Result<String> {
        let mut facts = Vec::new();
        if !self.root.exists() {
            return Ok(revision(&[]));
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                facts.push((path.clone(), fs::metadata(path)?));
            }
        }
        let runs = self.root.join("runs");
        if runs.exists() {
            for entry in fs::read_dir(runs)? {
                let path = entry?.path();
                if !path.is_dir() {
                    continue;
                }
                for name in ["events.jsonl", "registration.json"] {
                    let file = path.join(name);
                    match fs::metadata(&file) {
                        Ok(meta) => facts.push((file, meta)),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        facts.sort_by(|a, b| a.0.cmp(&b.0));
        let mut hash = Sha256::new();
        for (path, meta) in facts {
            hash.update(path.to_string_lossy().as_bytes());
            hash.update(meta.len().to_le_bytes());
            hash.update(
                meta.modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .to_le_bytes(),
            );
        }
        Ok(format!("{:x}", hash.finalize()))
    }
    pub fn list(&self) -> Result<Vec<Definition>> {
        let directory = match fs::read_dir(&self.root) {
            Ok(directory) => directory,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut tasks = Vec::new();
        for entry in directory {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .context("invalid task filename")?;
            tasks.push(self.get(id)?);
        }
        tasks.sort_by(|a, b| a.task.id.cmp(&b.task.id));
        Ok(tasks)
    }
    /// None creates only; Some requires the exact observed revision.
    pub fn put(&self, task: &ScheduleTask, expected: Option<&str>) -> Result<Definition> {
        timing::validate(task)?;
        let path = self.path(&task.id)?;
        let _lock = self.lock()?;
        match expected {
            Some(expected) => {
                let current = self.get(&task.id)?;
                if current.revision != expected {
                    bail!("task changed; reload before editing");
                }
                if serde_json::to_value(&current.task)? == serde_json::to_value(task)? {
                    return Ok(current);
                }
            }
            None if path.exists() => bail!("task already exists"),
            _ => {}
        }
        let bytes = serde_json::to_vec_pretty(task)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;
        Ok(Definition {
            task: task.clone(),
            revision: revision(&bytes),
        })
    }
    pub fn remove(&self, id: &str, expected: &str) -> Result<()> {
        let _lock = self.lock()?;
        if self.get(id)?.revision != expected {
            bail!("task changed; reload before deleting");
        }
        fs::remove_file(self.path(id)?)?;
        Ok(())
    }
    fn run_dir(&self, id: &str) -> Result<PathBuf> {
        self.path(id)?;
        let dir = self.root.join("runs").join(id);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
    fn append(&self, id: &str, event: &RunEvent) -> Result<()> {
        let path = self.run_dir(id)?.join("events.jsonl");
        // Opened for read AND write rather than append-only. `LockFileEx` — what
        // `lock_exclusive` becomes on Windows — refuses a handle that carries only
        // FILE_APPEND_DATA with ERROR_ACCESS_DENIED, which made every run fact
        // unwritable there. Appending at the end is therefore reached by seeking
        // under the lock, so the offset is read after any other appender is done.
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;
        file.seek(SeekFrom::End(0))?;
        let mut bytes = serde_json::to_vec(event)?;
        if bytes.len() > 1024 * 1024 {
            bail!("run metadata event exceeds one MiB");
        }
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_data()?;
        Ok(())
    }
    pub fn history(&self, id: &str) -> Result<Vec<RunEvent>> {
        self.path(id)?;
        let file = match File::open(self.root.join("runs").join(id).join("events.jsonl")) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        fs2::FileExt::lock_shared(&file)?;
        BufReader::new(file)
            .lines()
            .map(|line| Ok(serde_json::from_str(&line?)?))
            .collect()
    }
    pub fn latest_event(&self, id: &str) -> Result<Option<RunEvent>> {
        self.path(id)?;
        let mut file = match File::open(self.root.join("runs").join(id).join("events.jsonl")) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        fs2::FileExt::lock_shared(&file)?;
        let end = file.metadata()?.len();
        Ok(line_before(&mut file, end)?
            .map(|(bytes, _)| serde_json::from_slice(&bytes))
            .transpose()?)
    }
    /// Cursor is an exclusive byte position in this append-only log, so newly
    /// appended runs cannot shift older pages. Read only the requested tail.
    pub fn history_page(
        &self,
        id: &str,
        before: Option<usize>,
        limit: usize,
    ) -> Result<HistoryPage> {
        if !(1..=100).contains(&limit) {
            bail!("history limit must be 1–100");
        }
        self.recover_interrupted(id)?;
        let mut file = match File::open(self.root.join("runs").join(id).join("events.jsonl")) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.get(id)?;
                return Ok(HistoryPage {
                    events: Vec::new(),
                    next_cursor: None,
                });
            }
            Err(e) => return Err(e.into()),
        };
        fs2::FileExt::lock_shared(&file)?;
        let end = file.metadata()?.len();
        let mut cursor = before.map(|v| v as u64).unwrap_or(end);
        if cursor > end {
            bail!("invalid history cursor; refresh run history");
        }
        let mut events = Vec::new();
        for _ in 0..limit {
            let Some((bytes, start)) = line_before(&mut file, cursor)? else {
                break;
            };
            events.push(serde_json::from_slice(&bytes)?);
            cursor = start;
        }
        events.reverse();
        Ok(HistoryPage {
            events,
            next_cursor: if cursor > 0 {
                Some(usize::try_from(cursor)?)
            } else {
                None
            },
        })
    }
    pub fn recover_interrupted(&self, id: &str) -> Result<()> {
        self.path(id)?;
        let dir = self.root.join("runs").join(id);
        match fs::metadata(&dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join("lease"))?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _catalog = self.lock()?;
                self.recover_locked(id)
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    fn recover_locked(&self, id: &str) -> Result<()> {
        if matches!(self.latest_event(id)?,Some(RunEvent::Finished {status,..}) if status!="rejected")
        {
            return Ok(());
        }
        // Acquiring the process lease proves no prior worker is still running.
        // An unclosed start therefore represents a crashed/interrupted process.
        let mut unfinished = std::collections::BTreeSet::new();
        for event in self.history(id)? {
            match event {
                RunEvent::Started { run_id, .. } => {
                    unfinished.insert(run_id);
                }
                RunEvent::Finished { run_id, .. } => {
                    unfinished.remove(&run_id);
                }
                RunEvent::Session { .. } => {}
            }
        }
        for run_id in unfinished {
            self.append(
                id,
                &RunEvent::Finished {
                    run_id,
                    status: "interrupted".into(),
                    error: Some("previous worker exited without a terminal record".into()),
                    at: now(),
                },
            )?;
        }
        Ok(())
    }
    /// A process-scoped lease excludes OS/manual/remote overlaps. It never
    /// updates the definition when finishing, so edits and deletions survive.
    pub fn begin(&self, id: &str) -> Result<RunLease> {
        self.begin_requested(id, None)
    }
    pub fn begin_requested(&self, id: &str, request_id: Option<&str>) -> Result<RunLease> {
        self.begin_revision(id, request_id, None)
    }
    pub fn request_claimed(&self, id: &str, request_id: &str) -> Result<bool> {
        self.path(id)?;
        if !valid_id(request_id) {
            bail!("invalid request id");
        }
        match fs::metadata(
            self.root
                .join("runs")
                .join(id)
                .join("requests")
                .join(request_id),
        ) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub fn record_rejected(&self, id: &str, request_id: &str, error: String) -> Result<()> {
        if !valid_id(request_id) {
            bail!("invalid run request id");
        }
        let _catalog = self.lock()?;
        if self.request_claimed(id, request_id)? {
            return Ok(());
        }
        self.history(id)?;
        let dir = self.run_dir(id)?.join("requests");
        fs::create_dir_all(&dir)?;
        let mut claim = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(dir.join(request_id))?;
        claim.write_all(b"rejected")?;
        claim.sync_all()?;
        self.append(
            id,
            &RunEvent::Finished {
                run_id: format!("manual:{request_id}"),
                status: "rejected".into(),
                error: Some(error),
                at: now(),
            },
        )
    }
    pub fn begin_revision(
        &self,
        id: &str,
        request_id: Option<&str>,
        expected: Option<&str>,
    ) -> Result<RunLease> {
        let dir = self.run_dir(id)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join("lease"))?;
        file.try_lock_exclusive()
            .context("task is already running")?;
        let _catalog = self.lock()?;
        let definition = self.get(id)?;
        if expected.is_some_and(|expected| expected != definition.revision) {
            bail!("task changed before execution; reload before running");
        }
        if !definition.task.enabled {
            bail!("task is disabled");
        }
        if let Some(request_id) = request_id {
            if !valid_id(request_id) || request_id.len() > 128 {
                bail!("invalid run request id");
            }
            let requests = dir.join("requests");
            fs::create_dir_all(&requests)?;
            // The durable claim is made before any model/tool work. If a
            // process dies after this point, retrying the same request never
            // silently executes a second time.
            let mut claim = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(requests.join(request_id))
                .context("run request already accepted; inspect its history")?;
            claim.write_all(now().to_string().as_bytes())?;
            claim.sync_all()?;
        }
        self.recover_locked(id)?;
        let run_id = request_id
            .map(|id| format!("manual:{id}"))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.append(
            id,
            &RunEvent::Started {
                run_id: run_id.clone(),
                task_id: id.to_string(),
                revision: definition.revision.clone(),
                prompt: definition.task.prompt.clone(),
                cwd: definition.task.cwd.clone(),
                at: now(),
            },
        )?;
        Ok(RunLease {
            store: self.clone(),
            definition,
            run_id,
            _file: file,
            finished: false,
            write_failed: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

pub struct RunLease {
    store: Store,
    pub definition: Definition,
    pub run_id: String,
    _file: File,
    finished: bool,
    write_failed: std::sync::atomic::AtomicBool,
}
impl RunLease {
    /// Latest conversation recorded for this task, paired with the cwd at its
    /// admission. Reads backwards one bounded record at a time; failed starts
    /// without a Session fact do not erase the existing binding.
    pub fn bound_session(&self) -> Result<Option<(String, String)>> {
        let path = self
            .store
            .run_dir(&self.definition.task.id)?
            .join("events.jsonl");
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        fs2::FileExt::lock_shared(&file)?;
        let mut cursor = file.metadata()?.len();
        let mut selected: Option<(String, String)> = None;
        while let Some((bytes, start)) = line_before(&mut file, cursor)? {
            cursor = start;
            match serde_json::from_slice::<RunEvent>(&bytes)? {
                RunEvent::Session {
                    run_id, session_id, ..
                } if selected.is_none() => {
                    if !valid_id(&session_id) {
                        bail!("invalid automation session id");
                    }
                    selected = Some((run_id, session_id));
                }
                RunEvent::Started {
                    run_id,
                    task_id,
                    cwd,
                    ..
                } if selected
                    .as_ref()
                    .is_some_and(|(selected, _)| *selected == run_id) =>
                {
                    if task_id != self.definition.task.id {
                        bail!("automation binding belongs to another task");
                    }
                    return Ok(selected.map(|(_, session)| (session, cwd)));
                }
                _ => {}
            }
        }
        if selected.is_some() {
            bail!("automation session has no matching run admission");
        }
        Ok(None)
    }
    pub fn session(&self, session_id: &str) -> Result<()> {
        if self.write_failed.load(std::sync::atomic::Ordering::SeqCst) {
            bail!("run metadata persistence is uncertain");
        }
        if !valid_id(session_id) {
            bail!("invalid automation session id");
        }
        let bound = self.bound_session().map_err(|error| {
            self.write_failed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            error
        })?;
        if bound.is_some_and(|(bound, _)| bound != session_id) {
            bail!("automation is already bound to another session");
        }
        let result = self.store.append(
            &self.definition.task.id,
            &RunEvent::Session {
                run_id: self.run_id.clone(),
                session_id: session_id.into(),
                at: now(),
            },
        );
        if result.is_err() {
            self.write_failed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        result
    }
    pub fn finish(mut self, status: &str, error: Option<String>) -> Result<()> {
        self.finished = true;
        if self.write_failed.load(std::sync::atomic::Ordering::SeqCst) {
            bail!("run metadata persistence is uncertain");
        }
        self.store.append(
            &self.definition.task.id,
            &RunEvent::Finished {
                run_id: self.run_id.clone(),
                status: status.to_string(),
                error,
                at: now(),
            },
        )?;
        self.finished = true;
        Ok(())
    }
}
impl Drop for RunLease {
    fn drop(&mut self) {
        if !self.finished && !self.write_failed.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = self.store.append(
                &self.definition.task.id,
                &RunEvent::Finished {
                    run_id: self.run_id.clone(),
                    status: "interrupted".into(),
                    error: Some("execution ended before a terminal result was recorded".into()),
                    at: now(),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn task() -> ScheduleTask {
        serde_json::from_value(serde_json::json!({ "id":"daily-test", "title":"Daily review", "prompt":"Review changes", "cwd":std::env::temp_dir(), "schedule":{"kind":"daily","time":"09:00"} })).unwrap()
    }
    #[test]
    fn recurring_runs_keep_their_conversation_across_failures_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let task = task();
        store.put(&task, None).unwrap();
        let first = store.begin(&task.id).unwrap();
        assert_eq!(first.bound_session().unwrap(), None);
        first.session("task-session").unwrap();
        assert!(first.session("different-session").is_err());
        // Interruption releases the execution lease but keeps the session fact.
        drop(first);
        let store = Store::new(dir.path());
        let failed = store.begin(&task.id).unwrap();
        assert_eq!(
            failed.bound_session().unwrap(),
            Some(("task-session".into(), task.cwd.clone()))
        );
        failed
            .finish("error", Some("provider unavailable".into()))
            .unwrap();
        let next = store.begin(&task.id).unwrap();
        assert_eq!(next.bound_session().unwrap().unwrap().0, "task-session");
        next.session("task-session").unwrap();
        assert!(store.begin(&task.id).is_err(), "same task cannot overlap");
        let mut other = task.clone();
        other.id = "other-task".into();
        store.put(&other, None).unwrap();
        let other = store.begin(&other.id).unwrap();
        assert_eq!(other.bound_session().unwrap(), None);
        other.session("other-session").unwrap();
        other.finish("ok", None).unwrap();
        next.finish("ok", None).unwrap();
        assert_eq!(
            store
                .begin(&task.id)
                .unwrap()
                .bound_session()
                .unwrap()
                .unwrap()
                .0,
            "task-session"
        );
    }

    #[test]
    fn old_tasks_adopt_the_latest_run_but_keep_the_binding_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let task = task();
        let definition = store.put(&task, None).unwrap();
        for (run, session) in [("old", "old-session"), ("recent", "recent-session")] {
            store
                .append(
                    &task.id,
                    &RunEvent::Started {
                        run_id: run.into(),
                        task_id: task.id.clone(),
                        revision: definition.revision.clone(),
                        prompt: task.prompt.clone(),
                        cwd: task.cwd.clone(),
                        at: 1,
                    },
                )
                .unwrap();
            store
                .append(
                    &task.id,
                    &RunEvent::Session {
                        run_id: run.into(),
                        session_id: session.into(),
                        at: 2,
                    },
                )
                .unwrap();
            store
                .append(
                    &task.id,
                    &RunEvent::Finished {
                        run_id: run.into(),
                        status: "ok".into(),
                        error: None,
                        at: 3,
                    },
                )
                .unwrap();
        }
        let mut changed = task.clone();
        changed.cwd = "/different-project".into();
        store.put(&changed, Some(&definition.revision)).unwrap();
        let run = store.begin(&task.id).unwrap();
        assert_eq!(
            run.bound_session().unwrap(),
            Some(("recent-session".into(), task.cwd))
        );
    }

    #[test]
    fn malformed_session_bindings_fail_instead_of_starting_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let task = task();
        store.put(&task, None).unwrap();
        let run = store.begin(&task.id).unwrap();
        assert!(run.session("../escape").is_err());
        store
            .append(
                &task.id,
                &RunEvent::Session {
                    run_id: "missing-admission".into(),
                    session_id: "safe".into(),
                    at: 1,
                },
            )
            .unwrap();
        assert!(run.bound_session().is_err());
    }

    #[test]
    fn definitions_are_atomic_revision_checked_and_existing_json_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let first = store.put(&task(), None).unwrap();
        assert!(store.put(&task(), None).is_err());
        let mut edited = first.task.clone();
        edited.title = "Changed".into();
        let next = store.put(&edited, Some(&first.revision)).unwrap();
        assert_ne!(next.revision, first.revision);
        assert!(store.put(&task(), Some(&first.revision)).is_err());
        assert!(store.remove(&task().id, &first.revision).is_err());
        assert_eq!(
            Store::new(dir.path()).list().unwrap()[0].task.title,
            "Changed"
        );
    }
    #[test]
    fn finishing_a_run_never_overwrites_an_edit_or_resurrects_a_deleted_task() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let initial = store.put(&task(), None).unwrap();
        let run = store.begin(&task().id).unwrap();
        run.session("native-session").unwrap();
        let mut changed = initial.task;
        changed.prompt = "New instruction".into();
        let edited = store.put(&changed, Some(&initial.revision)).unwrap();
        run.finish("ok", None).unwrap();
        assert_eq!(
            store.get(&changed.id).unwrap().task.prompt,
            "New instruction"
        );
        let run = store.begin(&changed.id).unwrap();
        store.remove(&changed.id, &edited.revision).unwrap();
        run.finish("error", Some("provider unavailable".into()))
            .unwrap();
        assert!(store.list().unwrap().is_empty());
        let history = Store::new(dir.path()).history(&changed.id).unwrap();
        assert_eq!(history.len(), 5);
        assert!(
            matches!(&history[0], RunEvent::Started { prompt, .. } if prompt == "Review changes")
        );
        assert!(
            matches!(&history[1], RunEvent::Session { session_id, .. } if session_id == "native-session")
        );
    }
    #[test]
    fn process_lease_prevents_overlap_and_drop_records_interruption() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.put(&task(), None).unwrap();
        let lease = store.begin(&task().id).unwrap();
        assert!(Store::new(dir.path()).begin(&task().id).is_err());
        drop(lease);
        assert!(
            matches!(&store.history(&task().id).unwrap()[1], RunEvent::Finished { status, .. } if status == "interrupted")
        );
        store.begin(&task().id).unwrap().finish("ok", None).unwrap();
    }
    #[test]
    fn corruption_is_not_reported_as_an_empty_catalog_and_ids_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        fs::write(dir.path().join("broken.json"), b"{broken").unwrap();
        assert!(store.list().is_err());
        assert!(store.get("../outside").is_err());
        assert!(store.history("../outside").is_err());
    }
}

#[cfg(test)]
mod run_recovery_tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Store, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let task: ScheduleTask=serde_json::from_value(serde_json::json!({"id":"run-test","title":"Run","prompt":"Review","cwd":std::env::temp_dir(),"schedule":{"kind":"hourly"}})).unwrap();
        store.put(&task, None).unwrap();
        (dir, store, task.id)
    }
    #[test]
    fn a_finished_request_cannot_execute_again_with_the_same_key() {
        let (_dir, store, id) = fixture();
        store
            .begin_requested(&id, Some("request-1"))
            .unwrap()
            .finish("ok", None)
            .unwrap();
        assert!(store.begin_requested(&id, Some("request-1")).is_err());
        assert_eq!(store.history(&id).unwrap().len(), 2);
    }
    #[test]
    fn observing_a_crashed_worker_records_one_terminal_fact() {
        let (_dir, store, id) = fixture();
        store
            .append(
                &id,
                &RunEvent::Started {
                    run_id: "crashed-run".into(),
                    task_id: id.clone(),
                    revision: "old".into(),
                    prompt: "Original".into(),
                    cwd: "/tmp".into(),
                    at: 1,
                },
            )
            .unwrap();
        store.recover_interrupted(&id).unwrap();
        store.recover_interrupted(&id).unwrap();
        assert_eq!(store.history(&id).unwrap().len(), 2);
        assert!(
            matches!(&store.history(&id).unwrap()[1],RunEvent::Finished {status,..} if status=="interrupted")
        );
    }
    #[test]
    fn cancelled_admission_writes_no_definition() {
        let (_dir, store, _id) = fixture();
        let service = service::Service::new(store.clone(), None);
        let mut task = store.list().unwrap()[0].task.clone();
        task.id = "not-created".into();
        assert!(service
            .apply_if(
                "cancelled-request",
                &service::Mutation::Create { task },
                || true
            )
            .is_err());
        assert_eq!(store.list().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod history_page_tests {
    use super::*;
    #[test]
    fn paging_cursor_stays_stable_when_a_new_run_is_appended() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let task: ScheduleTask=serde_json::from_value(serde_json::json!({"id":"history-test","title":"History","prompt":"Original","cwd":std::env::temp_dir(),"schedule":{"kind":"hourly"}})).unwrap();
        let initial = store.change_stamp().unwrap();
        store.put(&task, None).unwrap();
        assert_ne!(store.change_stamp().unwrap(), initial);
        let first = store.begin(&task.id).unwrap();
        let first_id = first.run_id.clone();
        first.finish("ok", None).unwrap();
        store
            .begin(&task.id)
            .unwrap()
            .finish("error", Some("failed".into()))
            .unwrap();
        let page = store.history_page(&task.id, None, 2).unwrap();
        assert!(page.next_cursor.is_some());
        let stamp = store.change_stamp().unwrap();
        store.begin(&task.id).unwrap().finish("ok", None).unwrap();
        assert_ne!(store.change_stamp().unwrap(), stamp);
        let old = store.history_page(&task.id, page.next_cursor, 2).unwrap();
        assert!(matches!(&old.events[0],RunEvent::Started {run_id,..} if run_id==&first_id));
        assert_eq!(old.next_cursor, None);
        assert!(store.history_page(&task.id, Some(999), 20).is_err());
        assert!(store.history_page(&task.id, None, 0).is_err());
    }
}

#[cfg(test)]
mod persistence_failure_tests {
    use super::*;
    #[test]
    fn a_failed_metadata_write_stops_all_further_writes_from_that_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let task:ScheduleTask=serde_json::from_value(serde_json::json!({"id":"uncertain","title":"Uncertain","prompt":"Review","cwd":std::env::temp_dir(),"schedule":{"kind":"hourly"}})).unwrap();
        store.put(&task, None).unwrap();
        let lease = store.begin(&task.id).unwrap();
        let log = dir.path().join("runs/uncertain/events.jsonl");
        let saved = log.with_extension("saved");
        fs::rename(&log, &saved).unwrap();
        fs::create_dir(&log).unwrap();
        assert!(lease.session("not-committed").is_err());
        fs::remove_dir(&log).unwrap();
        fs::rename(&saved, &log).unwrap();
        assert!(lease.finish("error", Some("failed".into())).is_err());
        assert_eq!(
            store.history(&task.id).unwrap().len(),
            1,
            "no retry or Drop write after the first failure"
        );
    }
    #[test]
    fn platform_device_names_and_path_escapes_are_not_task_or_operation_ids() {
        for id in [
            "..",
            "../task",
            "CON",
            "con.json",
            "AUX",
            "LPT1",
            "com9.data",
            "x/y",
            "x\\y",
        ] {
            assert!(!valid_id(id), "{id}");
        }
        assert!(valid_id("daily-review-abc123"));
    }
}
