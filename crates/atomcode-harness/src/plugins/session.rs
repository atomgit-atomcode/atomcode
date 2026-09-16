//! The session domain: the log, the projection registry, and a durable store.
//!
//! Three rows rather than one, because they are three decisions. A deployment
//! can keep the log and drop persistence (an eval run), or keep persistence and
//! swap the store (a database instead of JSONL), without any of them knowing.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::events::{AgentCreated, AgentInfo, SessionEventCommitted};
use crate::seams::{
    SessionDefaults, SessionDefaultsSvc, SessionPersistence, SessionPersistenceSvc,
    SessionProjectionsSvc,
};
use crate::session::{
    Committed, LoggedEvent, ProjectionUnit, SessionEvent, SessionHeader, SessionProjections,
};

#[derive(Debug, Deserialize, Default)]
struct SessionRow {
    /// Explicit id for the front end's own agent, so a caller can resume a
    /// known session. Minted when absent.
    #[serde(default)]
    id: Option<String>,
    /// Replay `id`'s stored log before the first turn. There is no separate
    /// snapshot format: the log *is* the snapshot, so a resume is a replay.
    #[serde(default)]
    resume: bool,
}

/// The `session` row.
///
/// It used to provide the tree's one log, and every agent shared it. Now a log
/// belongs to an agent — created with it, provided into its realm, torn down
/// with it — and this row only says how the front end's own agent gets its
/// identity. See [`crate::agent::CreateAgent::root`].
pub struct SessionPlugin;

#[async_trait]
impl Plugin for SessionPlugin {
    fn name(&self) -> &'static str {
        "session"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-defaults"]
    }
    fn description(&self) -> &'static str {
        "which session the front end's own agent gets, and whether to resume it"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: SessionRow = parse(config)?;
        // This row's own two knobs, and nothing about how they get set — a
        // launcher that writes them says so itself.
        crate::plugins::self_knowledge::describes(
            ctx,
            "session-identity",
            10,
            "SESSION IDENTITY. The tree's own agent takes its session from the \
             `session` row: `id` names it (minted when absent), and `resume = true` \
             continues that session by replaying the events `session-persistence` \
             stored for it before the first turn. There is no separate snapshot \
             format for this: the stored events are the snapshot.",
        );
        let _ = ctx
            .provide::<SessionDefaultsSvc>(Arc::new(SessionDefaults {
                id: row.id,
                resume: row.resume,
                seed: Vec::new(),
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct TurnBoundary;

impl ProjectionUnit for TurnBoundary {
    fn key(&self) -> &'static str {
        "turnBoundary"
    }
    fn fold(&self, state: &mut Value, event: &SessionEvent) {
        let turns = state
            .as_object_mut()
            .is_none()
            .then(|| *state = json!({ "turns": [] }));
        let _ = turns;
        let Some(list) = state.get_mut("turns").and_then(Value::as_array_mut) else {
            return;
        };
        match event {
            SessionEvent::TurnStart { turn } => {
                list.push(json!({ "turn": turn, "rounds": 0, "stop": null }));
            }
            SessionEvent::RequestHeader { .. } => {
                if let Some(last) = list.last_mut().and_then(Value::as_object_mut) {
                    let rounds = last.get("rounds").and_then(Value::as_u64).unwrap_or(0);
                    last.insert("rounds".into(), json!(rounds + 1));
                }
            }
            SessionEvent::TurnEnd { stop, .. } => {
                if let Some(last) = list.last_mut().and_then(Value::as_object_mut) {
                    last.insert("stop".into(), json!(stop));
                }
            }
            _ => {}
        }
    }
}

/// Cumulative token usage. A separate unit because it has a separate owner and
/// a separate lifetime — dropping the row drops the accounting, nothing else.
struct TokenTotals;

impl ProjectionUnit for TokenTotals {
    fn key(&self) -> &'static str {
        "tokenTotals"
    }
    fn fold(&self, state: &mut Value, event: &SessionEvent) {
        if !state.is_object() {
            *state = json!({ "prompt": 0, "completion": 0, "cached": 0 });
        }
        let SessionEvent::Usage { usage, .. } = event else {
            return;
        };
        let Some(object) = state.as_object_mut() else {
            return;
        };
        for (key, delta) in [
            ("prompt", usage.prompt as u64),
            ("completion", usage.completion as u64),
            ("cached", usage.cached as u64),
        ] {
            let current = object.get(key).and_then(Value::as_u64).unwrap_or(0);
            object.insert(key.into(), json!(current + delta));
        }
    }
}

pub struct SessionProjectionsPlugin;

#[async_trait]
impl Plugin for SessionProjectionsPlugin {
    fn name(&self) -> &'static str {
        "session-projection"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-projections"]
    }
    fn description(&self) -> &'static str {
        "incremental folds over the log, read by key"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let projections = Arc::new(SessionProjections::new());
        projections.register(Arc::new(TurnBoundary));
        projections.register(Arc::new(TokenTotals));
        let _ = ctx
            .provide::<SessionProjectionsSvc>(projections)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- persistence --------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct PersistenceRow {
    /// Directory holding the per-project session buckets. Defaults under the
    /// harness home.
    #[serde(default)]
    root: Option<String>,
    /// Which project this session belongs to. Sessions bucket by it, so
    /// "what did we do on *this* repo" is answerable at all — a flat directory
    /// has no such dimension and no index can invent one.
    ///
    /// Defaults to the process cwd.
    #[serde(default)]
    project_root: Option<String>,
}

pub(crate) struct JsonlStore {
    root: PathBuf,
    /// The project bucket new sessions are written into.
    bucket: String,
}

impl JsonlStore {
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
    /// Read-only and deliberately not migrated: a resume must keep working for
    /// a log the user already has, and rewriting a hundred files on disk to
    /// tidy a layout is a much worse trade than reading two paths.
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

    /// The header this store holds for `session_id`, read synchronously: what a
    /// description reports as the session's recorded start, which is not the
    /// in-memory log's when the log was seeded rather than resumed from here.
    fn stored_header(&self, session_id: &str) -> Option<SessionHeader> {
        use std::io::BufRead;
        let path = match self.path(session_id) {
            p if p.exists() => p,
            _ => self.legacy_path(session_id),
        };
        let file = std::fs::File::open(&path).ok()?;
        let mut first = String::new();
        std::io::BufReader::new(file).read_line(&mut first).ok()?;
        Self::parse_header(&path, &first).ok().flatten()
    }

    /// Write the header line if the file does not exist yet. Synchronous and
    /// small on purpose: it must be on disk before the first event lands, and
    /// the first event is appended from a task this does not wait for.
    fn begin_sync(&self, header: &SessionHeader) -> Result<(), String> {
        let path = self.path(&header.id);
        if path.exists() || self.legacy_path(&header.id).exists() {
            return Ok(());
        }
        std::fs::create_dir_all(self.dir()).map_err(|e| e.to_string())?;
        let line = serde_json::json!({ "header": header });
        let mut text = serde_json::to_string(&line).map_err(|e| e.to_string())?;
        text.push('\n');
        std::fs::write(&path, text).map_err(|e| e.to_string())
    }

    fn parse(path: &std::path::Path, text: &str) -> Result<Vec<LoggedEvent>, String> {
        let mut events = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|e| format!("{}:{}: {e}", path.display(), index + 1))?;
            // The header line carries no event; it is read by `parse_header`.
            if value.get("header").is_some() && value.get("event").is_none() {
                continue;
            }
            let seq = value.get("seq").and_then(Value::as_u64).unwrap_or(0);
            let event: SessionEvent =
                serde_json::from_value(value.get("event").cloned().unwrap_or(Value::Null))
                    .map_err(|e| format!("{}:{}: {e}", path.display(), index + 1))?;
            events.push(LoggedEvent { seq, event });
        }
        Ok(events)
    }

    /// Read one session's events from a path, for a reader that already knows
    /// which file it wants (the recall row walking the bucket).
    pub(crate) fn read_at(path: &std::path::Path) -> Result<Vec<LoggedEvent>, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Self::parse(path, &text)
    }
}

#[async_trait]
impl SessionPersistence for JsonlStore {
    fn location(&self, session_id: &str) -> Option<String> {
        Some(self.path(session_id).display().to_string())
    }
    async fn begin(&self, header: &SessionHeader) -> Result<(), String> {
        self.begin_sync(header)
    }
    async fn header(&self, session_id: &str) -> Result<Option<SessionHeader>, String> {
        let path = match self.path(session_id) {
            p if p.exists() => p,
            _ => self.legacy_path(session_id),
        };
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        Self::parse_header(&path, &text)
    }

    async fn append(&self, session_id: &str, events: &[LoggedEvent]) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(self.dir()).map_err(|e| e.to_string())?;
        let mut buffer = String::new();
        for logged in events {
            let line = serde_json::json!({ "seq": logged.seq, "event": logged.event });
            buffer.push_str(&serde_json::to_string(&line).map_err(|e| e.to_string())?);
            buffer.push('\n');
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(session_id))
            .map_err(|e| e.to_string())?;
        file.write_all(buffer.as_bytes()).map_err(|e| e.to_string())
    }

    async fn load(&self, session_id: &str) -> Result<Vec<LoggedEvent>, String> {
        // The bucket first, then where sessions lived before buckets existed.
        let path = match self.path(session_id) {
            p if p.exists() => p,
            _ => self.legacy_path(session_id),
        };
        if !path.exists() {
            return Ok(Vec::new());
        }
        Self::read_at(&path)
    }

    async fn list(&self) -> Result<Vec<String>, String> {
        // This project's sessions, not every session on the machine. A list
        // that crossed projects would make "what did we do here" unanswerable
        // by the same amount the flat layout did.
        let Ok(entries) = std::fs::read_dir(self.dir()) else {
            return Ok(Vec::new());
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
        Ok(ids)
    }
}

pub struct SessionPersistenceJsonlPlugin;

#[async_trait]
impl Plugin for SessionPersistenceJsonlPlugin {
    fn name(&self) -> &'static str {
        "session-persistence-jsonl"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["session-persistence", "operations", "agents"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-persistence"]
    }
    fn description(&self) -> &'static str {
        "append every committed event to <root>/<session-id>.jsonl"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: PersistenceRow = parse(config)?;
        let root = row
            .root
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::home().join("sessions"));
        let project = row
            .project_root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        // The same bucket id the rest of the stack uses, so the two session
        // stores under this root stop sharing one level and start agreeing on
        // what a project is.
        let bucket = atomcode_config::util::stable_project_hash(&project);
        let store = Arc::new(JsonlStore { root, bucket });
        // What this row does, and only that. It used to go on to explain resume
        // and recall, which are other rows' — and in an assembly that keeps its
        // sessions elsewhere and swaps both of those out, this entry was left
        // telling the agent its journal was the session store.
        crate::plugins::self_knowledge::describes(
            ctx,
            "session-event-log",
            12,
            format!(
                "SESSION EVENT LOG. Every committed fact of a session is appended \
                 to `{}/<session-id>.jsonl`, one directory per project — this one \
                 is this project's.",
                store.dir().display()
            ),
        );
        let facts_store = store.clone();
        let facts_ctx = ctx.clone();
        crate::plugins::self_knowledge::describes_live(
            ctx,
            crate::seams::Aspect::Session,
            "session-event-log/this-session",
            12,
            move |asking| {
                use crate::agent::OnlySession;
                let log = asking.only_session()?;
                let persisted = facts_ctx
                    .service::<crate::seams::AgentsSvc>()
                    .and_then(|agents| agents.by_session(log.id()))
                    .map(|agent| agent.persist())
                    .unwrap_or(true);
                if !persisted {
                    return None;
                }
                let mut lines = vec![format!(
                    "event log: {}",
                    facts_store.path(log.id()).display()
                )];
                if let Some(header) = facts_store.stored_header(log.id()) {
                    lines.push(format!(
                        "  started at: {} (unix ms, as the log recorded it)",
                        header.created_at
                    ));
                    if let Some(cwd) = &header.cwd {
                        lines.push(format!("  working directory: {cwd}"));
                    }
                    if let Some(parent) = &header.parent {
                        lines.push(format!(
                            "  forked from: {parent} (the first {} events are inherited)",
                            header.inherited
                        ));
                    }
                }
                Some(lines.join("\n"))
            },
        );
        let _ = ctx
            .provide::<SessionPersistenceSvc>(store.clone())
            .map_err(|e| e.to_string())?;

        // The store is fed by listening, not by the loop calling it. That is
        // what lets persistence be removed without the loop changing.
        //
        // Every agent's log, by the id the fact carries — except a delegated
        // child's, whose transcript is its parent's business and not a session
        // of its own. The agent is resolved live rather than captured: it may
        // be gone by the time its last fact is flushed, and persisting that
        // fact is still right.
        // A session's header goes down the moment its agent exists — before
        // any event, which is the only order in which "first line" is true.
        // Inline rather than spawned for the same reason.
        let header_ctx = ctx.clone();
        let header_store = store.clone();
        let _ = ctx.on_emit::<AgentCreated>(move |created: &AgentInfo| {
            let Some(agent) = header_ctx
                .service::<crate::seams::AgentsSvc>()
                .and_then(|a| a.get(created.id))
            else {
                return;
            };
            if !agent.persist() {
                return;
            }
            if let Err(e) = header_store.begin_sync(agent.session().header()) {
                eprintln!("session-persistence: {e}");
            }
        });

        // ONE writer, fed by a queue — not a task per event.
        //
        // The previous shape spawned a `tokio::spawn` per committed fact, for a
        // good reason (a slow disk must not stall the turn) and with a bad
        // consequence: the tasks raced, so the file was written in whatever
        // order the scheduler happened to pick. Sequence numbers are minted in
        // order at commit; the LINES were not in that order. Measured on real
        // sessions: ~10-18% of all facts out of order, and 2-10 of the
        // MODEL-VISIBLE ones per session — and nothing sorts on the way back in
        // (`JsonlStore::parse` reads lines in file order, `SessionLog::restore`
        // stores them as given, `derive_messages` folds them as stored). A
        // resumed conversation could therefore show the model two tool results
        // swapped, or context after the message it was meant to precede.
        //
        // A queue keeps the property that mattered — the turn never waits on the
        // disk, because `send` is non-blocking — and restores the one that was
        // lost, because a single consumer appends in the order it receives.
        // Same shape as `capabilities::datalog::DatalogWriter`, for the same
        // reason.
        //
        // The task ends when the sender drops, which happens when this row
        // unloads: no shutdown handshake, and nothing left writing into a store
        // whose row is gone.
        let (writes, mut queue) =
            tokio::sync::mpsc::unbounded_channel::<(String, crate::session::LoggedEvent)>();
        let writer_store = store.clone();
        tokio::spawn(async move {
            while let Some((id, logged)) = queue.recv().await {
                if let Err(e) = writer_store
                    .append(&id, std::slice::from_ref(&logged))
                    .await
                {
                    eprintln!("session-persistence: {e}");
                }
            }
        });

        let agents_ctx = ctx.clone();
        let _ = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            let keep = agents_ctx
                .service::<crate::seams::AgentsSvc>()
                .and_then(|a| a.by_session(&committed.session))
                .map(|a| a.persist())
                .unwrap_or(true);
            if !keep {
                return;
            }
            // Non-blocking, so the turn still never waits on the disk. A closed
            // channel means the row is unloading; the fact is dropped rather
            // than reported, because "we are shutting down" is not a failure.
            let _ = writes.send((committed.session.clone(), committed.logged()));
        });
        Ok(())
    }
}

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}
