//! `session-journal`: this product's event journal, in a directory of its own.
//!
//! The harness's `session-persistence-jsonl` row writes `<root>/<bucket>/<id>.jsonl`
//! and defaults `root` to `<home>/sessions` — which in AtomCode is the native
//! session store's root. Every tree this crate assembles (the coding runtime, and
//! `atui` through `CODING_DEFAULTS`) therefore dropped journal files into the
//! native project buckets, and each reader of native files tripped over them: the
//! session list reported half-sessions on every refresh, and one journal made
//! `recall` fail for the whole project.
//!
//! The harness row is left as it is; this product swaps it for this one. Same file
//! format — a header line, then one `{"seq","event"}` line per fact — so anything
//! that reads a harness journal reads this one. What differs is where:
//! `<home>/sessions/<SessionManager::JOURNAL_DIR>/<bucket>/<id>.jsonl`, the one
//! directory the native catalog passes over. A session already journaled in a
//! native bucket is still found there, and keeps growing where it is rather than
//! being split across two files; nothing new is written there.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::session::SessionManager;
use atomcode_harness::events::{AgentCreated, AgentInfo, SessionEventCommitted};
use atomcode_harness::seams::{AgentsSvc, SessionPersistence, SessionPersistenceSvc};
use atomcode_harness::session::{
    Committed, LoggedEvent, SessionEvent, SessionHeader, SESSION_FORMAT_VERSION,
};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, Default)]
struct JournalRow {
    /// Which project the journal belongs to; its bucket is that project's, the
    /// same one the native store uses. Defaults to the process cwd.
    #[serde(default)]
    project_root: Option<String>,
}

pub(crate) struct Journal {
    /// `<home>/sessions/<JOURNAL_DIR>/<bucket>`: where new sessions go.
    dir: PathBuf,
    /// `<home>/sessions/<bucket>`: where journals were written before this row,
    /// shared with native files.
    earlier: PathBuf,
}

impl Journal {
    fn for_project(project: &Path) -> Self {
        let sessions = atomcode_harness::home().join("sessions");
        let bucket = SessionManager::project_hash(project);
        Self {
            dir: sessions.join(SessionManager::JOURNAL_DIR).join(&bucket),
            earlier: sessions.join(&bucket),
        }
    }

    /// One path segment, whatever the id claims to be: a resumed id arrives from
    /// outside, and a crafted one must not escape the directory.
    fn file_name(session_id: &str) -> String {
        let safe: String = session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("{safe}.jsonl")
    }

    /// Where `session_id` is journaled: its existing file if it has one, here
    /// or where journals used to go, else where a new one is written.
    fn locate(&self, session_id: &str) -> PathBuf {
        let name = Self::file_name(session_id);
        let here = self.dir.join(&name);
        if here.exists() {
            return here;
        }
        let earlier = self.earlier.join(&name);
        if SessionManager::is_journal_file(&earlier) {
            return earlier;
        }
        here
    }

    fn begin_sync(&self, header: &SessionHeader) -> Result<(), String> {
        let path = self.locate(&header.id);
        if path.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let mut line = serde_json::to_string(&serde_json::json!({ "header": header }))
            .map_err(|e| e.to_string())?;
        line.push('\n');
        std::fs::write(&path, line).map_err(|e| e.to_string())
    }

    fn read_header(path: &Path) -> Result<Option<SessionHeader>, String> {
        use std::io::BufRead;
        let Ok(file) = std::fs::File::open(path) else {
            return Ok(None);
        };
        let mut first = String::new();
        std::io::BufReader::new(file)
            .read_line(&mut first)
            .map_err(|e| e.to_string())?;
        if first.trim().is_empty() {
            return Ok(None);
        }
        let value: Value =
            serde_json::from_str(&first).map_err(|e| format!("{}:1: {e}", path.display()))?;
        let Some(header) = value.get("header") else {
            return Ok(None);
        };
        let header: SessionHeader = serde_json::from_value(header.clone())
            .map_err(|e| format!("{}:1: header: {e}", path.display()))?;
        if header.version > SESSION_FORMAT_VERSION {
            return Err(format!(
                "{}: session format {} is newer than this build reads ({SESSION_FORMAT_VERSION})",
                path.display(),
                header.version,
            ));
        }
        Ok(Some(header))
    }

    fn read_events(path: &Path) -> Result<Vec<LoggedEvent>, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut events = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|e| format!("{}:{}: {e}", path.display(), index + 1))?;
            if value.get("header").is_some() && value.get("event").is_none() {
                continue;
            }
            let seq = value.get("seq").and_then(Value::as_u64).unwrap_or(0);
            let event: SessionEvent =
                serde_json::from_value(value.get("event").cloned().unwrap_or(Value::Null))
                    .map_err(|e| format!("{}:{}: {e}", path.display(), index + 1))?;
            let at = value.get("at").and_then(Value::as_u64).unwrap_or(0);
            events.push(LoggedEvent { seq, at, event });
        }
        Ok(events)
    }
}

#[async_trait]
impl SessionPersistence for Journal {
    fn location(&self, session_id: &str) -> Option<String> {
        Some(self.locate(session_id).display().to_string())
    }

    async fn begin(&self, header: &SessionHeader) -> Result<(), String> {
        self.begin_sync(header)
    }

    async fn header(&self, session_id: &str) -> Result<Option<SessionHeader>, String> {
        Self::read_header(&self.locate(session_id))
    }

    async fn append(&self, session_id: &str, events: &[LoggedEvent]) -> Result<(), String> {
        use std::io::Write;
        if events.is_empty() {
            return Ok(());
        }
        let path = self.locate(session_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut buffer = String::new();
        for logged in events {
            let line =
                serde_json::json!({ "seq": logged.seq, "at": logged.at, "event": logged.event });
            buffer.push_str(&serde_json::to_string(&line).map_err(|e| e.to_string())?);
            buffer.push('\n');
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?
            .write_all(buffer.as_bytes())
            .map_err(|e| e.to_string())
    }

    async fn load(&self, session_id: &str) -> Result<Vec<LoggedEvent>, String> {
        let path = self.locate(session_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        Self::read_events(&path)
    }

    async fn list(&self) -> Result<Vec<String>, String> {
        let mut ids = std::collections::BTreeSet::new();
        for (dir, shared) in [(&self.dir, false), (&self.earlier, true)] {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for path in entries.flatten().map(|entry| entry.path()) {
                if path.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                // The earlier directory is the native store's bucket: only its
                // journals are this store's sessions.
                if shared && !SessionManager::is_journal_file(&path) {
                    continue;
                }
                if let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) {
                    ids.insert(id.to_string());
                }
            }
        }
        Ok(ids.into_iter().rev().collect())
    }
}

/// See the module docs.
pub struct SessionJournalPlugin;

#[async_trait]
impl Plugin for SessionJournalPlugin {
    fn name(&self) -> &'static str {
        "session-journal"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["session-persistence", "operations", "agents"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-persistence"]
    }
    fn description(&self) -> &'static str {
        "append every committed event to the journal directory beside the native session store"
    }

    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: JournalRow = if config.is_null() {
            JournalRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let project = row
            .project_root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let store = Arc::new(Journal::for_project(&project));
        describe(ctx, &store);
        let _ = ctx
            .provide::<SessionPersistenceSvc>(store.clone())
            .map_err(|e| e.to_string())?;

        // A session's header goes down the moment its agent exists, before any
        // event, and inline for the same reason.
        let header_ctx = ctx.clone();
        let header_store = store.clone();
        let _ = ctx.on_emit::<AgentCreated>(move |created: &AgentInfo| {
            let Some(agent) = header_ctx
                .service::<AgentsSvc>()
                .and_then(|agents| agents.get(created.id))
            else {
                return;
            };
            if !agent.persist() {
                return;
            }
            if let Err(e) = header_store.begin_sync(agent.session().header()) {
                eprintln!("session-journal: {e}");
            }
        });

        // One writer fed by a queue, so lines land in commit order while the turn
        // never waits on the disk — the property the harness row learned to keep.
        let (writes, mut queue) = tokio::sync::mpsc::unbounded_channel::<(String, LoggedEvent)>();
        let writer_store = store.clone();
        tokio::spawn(async move {
            while let Some((id, logged)) = queue.recv().await {
                if let Err(e) = writer_store
                    .append(&id, std::slice::from_ref(&logged))
                    .await
                {
                    eprintln!("session-journal: {e}");
                }
            }
        });
        let agents_ctx = ctx.clone();
        let _ = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            let keep = agents_ctx
                .service::<AgentsSvc>()
                .and_then(|agents| agents.by_session(&committed.session))
                .map(|agent| agent.persist())
                .unwrap_or(true);
            if keep {
                let _ = writes.send((committed.session.clone(), committed.logged()));
            }
        });
        Ok(())
    }
}

/// What this row does, said by this row.
fn describe(ctx: &Context, store: &Arc<Journal>) {
    use atomcode_harness::plugins::self_knowledge::{describes, describes_live};
    describes(
        ctx,
        "session-event-log",
        12,
        format!(
            "SESSION EVENT LOG. Every committed fact of a session is also appended to \
             `{}/<session-id>.jsonl`, this project's directory in the event journal. \
             It is a record of what happened, kept apart from the session store.",
            store.dir.display()
        ),
    );
    let facts = store.clone();
    let facts_ctx = ctx.clone();
    describes_live(
        ctx,
        atomcode_harness::seams::Aspect::Session,
        "session-event-log/this-session",
        12,
        move |asking| {
            use atomcode_harness::agent::OnlySession;
            let log = asking.only_session()?;
            let persisted = facts_ctx
                .service::<AgentsSvc>()
                .and_then(|agents| agents.by_session(log.id()))
                .map(|agent| agent.persist())
                .unwrap_or(true);
            persisted.then(|| format!("event log: {}", facts.locate(log.id()).display()))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(root: &Path) -> Journal {
        Journal {
            dir: root.join("harness").join("0123456789abcdef"),
            earlier: root.join("0123456789abcdef"),
        }
    }

    fn header(id: &str) -> SessionHeader {
        SessionHeader::new(id)
    }

    #[tokio::test]
    async fn a_new_session_is_journaled_outside_the_native_bucket() {
        let root = tempfile::tempdir().unwrap();
        let journal = journal(root.path());
        journal.begin(&header("1789000000000-1")).await.unwrap();
        journal
            .append(
                "1789000000000-1",
                &[LoggedEvent {
                    seq: 1,
                    at: 0,
                    event: SessionEvent::TurnStart { turn: 1 },
                }],
            )
            .await
            .unwrap();

        assert!(journal.dir.join("1789000000000-1.jsonl").exists());
        assert!(
            !root
                .path()
                .join("0123456789abcdef")
                .join("1789000000000-1.jsonl")
                .exists(),
            "nothing new goes into the native bucket"
        );
        assert_eq!(journal.load("1789000000000-1").await.unwrap().len(), 1);
        assert_eq!(journal.list().await.unwrap(), vec!["1789000000000-1"]);
    }

    #[tokio::test]
    async fn a_session_journaled_in_the_native_bucket_is_found_and_kept_whole() {
        let root = tempfile::tempdir().unwrap();
        let journal = journal(root.path());
        std::fs::create_dir_all(&journal.earlier).unwrap();
        let old = journal.earlier.join("1700000000000-9.jsonl");
        std::fs::write(
            &old,
            "{\"header\":{\"version\":1,\"id\":\"1700000000000-9\",\"created_at\":1,\"inherited\":0}}\n\
             {\"seq\":1,\"event\":{\"kind\":\"turn_start\",\"turn\":1}}\n",
        )
        .unwrap();
        // A native transcript beside it is not a journal and not listed.
        std::fs::write(
            journal
                .earlier
                .join("5b0e0b8e-0000-4000-8000-000000000000.jsonl"),
            "{\"v\":1,\"ts\":1,\"session_id\":\"5b0e0b8e\",\"turn_id\":1}\n",
        )
        .unwrap();

        assert_eq!(journal.list().await.unwrap(), vec!["1700000000000-9"]);
        assert_eq!(
            journal
                .header("1700000000000-9")
                .await
                .unwrap()
                .map(|h| h.id),
            Some("1700000000000-9".to_string())
        );
        journal
            .append(
                "1700000000000-9",
                &[LoggedEvent {
                    seq: 2,
                    at: 0,
                    event: SessionEvent::TurnStart { turn: 2 },
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            journal.load("1700000000000-9").await.unwrap().len(),
            2,
            "it keeps growing where it already is, in one file"
        );
        assert!(!journal.dir.join("1700000000000-9.jsonl").exists());
    }
}
