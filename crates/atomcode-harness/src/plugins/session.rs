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

use crate::events::SessionEventCommitted;
use crate::seams::{SessionPersistence, SessionPersistenceSvc, SessionProjectionsSvc, SessionSvc};
use crate::session::{
    Committed, LoggedEvent, ProjectionUnit, SessionEvent, SessionLog, SessionProjections,
};

#[derive(Debug, Deserialize, Default)]
struct SessionRow {
    /// Explicit id, so a caller can resume a known session. Minted when absent.
    #[serde(default)]
    id: Option<String>,
}

pub struct SessionPlugin;

#[async_trait]
impl Plugin for SessionPlugin {
    fn name(&self) -> &'static str {
        "session"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["sessions"]
    }
    fn description(&self) -> &'static str {
        "the append-only session log everything else is derived from"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: SessionRow = parse(config)?;
        let id = row.id.unwrap_or_else(mint_session_id);
        let _ = ctx
            .provide::<SessionSvc>(Arc::new(SessionLog::new(id)))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn mint_session_id() -> String {
    // Wall clock plus pid: unique enough to name a file, and readable in a
    // directory listing, which is what session ids are actually for.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{now}-{}", std::process::id())
}

// ---- projections --------------------------------------------------------

/// Turn boundaries: where each turn started and how it ended. The loop needs
/// it, a UI needs it, a compactor needs it — so it is one shared unit rather
/// than three scans of the log.
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
    /// Load the session's existing events at mount time.
    ///
    /// There is no separate snapshot format to restore from: the log *is* the
    /// snapshot. Everything a resumed session needs — the model's view, the
    /// turn numbering, the compaction boundaries — is derived from the same
    /// events a live session appends, so a resume is a replay and not a second
    /// representation that can drift from the first.
    #[serde(default)]
    resume: bool,
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

    fn parse(path: &std::path::Path, text: &str) -> Result<Vec<LoggedEvent>, String> {
        let mut events = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|e| format!("{}:{}: {e}", path.display(), index + 1))?;
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
    fn inject(&self) -> &'static [&'static str] {
        &["sessions"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["session-persistence", "operations"]
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
        crate::plugins::self_knowledge::describes(
            ctx,
            "sessions",
            10,
            format!(
                "SESSIONS. Every committed fact of this session is appended to \n\
                 `{}`. Sessions are bucketed per project, so the `recall` tool \n\
                 searches this project's history and not the whole machine's.\n\
                 To resume one: set `resume = true` on the \
                 `session-persistence-jsonl` row and `id = \"<session-id>\"` on \
                 the `session` row. There is no snapshot format — the log IS the \
                 snapshot, so a resume is a replay.",
                store.dir().display()
            ),
        );
        let _ = ctx
            .provide::<SessionPersistenceSvc>(store.clone())
            .map_err(|e| e.to_string())?;

        // The store is fed by listening, not by the loop calling it. That is
        // what lets persistence be removed without the loop changing.
        let session = ctx.require::<SessionSvc>().map_err(|e| e.to_string())?;
        let id = session.id().to_string();

        if row.resume {
            let events = store.load(&id).await?;
            if !events.is_empty() {
                let turns = events
                    .iter()
                    .filter(|e| matches!(e.event, SessionEvent::TurnStart { .. }))
                    .count();
                // Sequence numbers and the turn counter come from the data, not
                // re-minted: a transcript keyed by (session, turn) would collect
                // duplicate keys after the first resume otherwise.
                session.restore(events);
                eprintln!(
                    "\x1b[2mresumed `{id}` — {turns} turn(s), {} event(s)\x1b[0m",
                    session.len()
                );
            }
        }
        let _ = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            // A delegated child commits to its own log, and this listener is
            // above both. Writing its facts here would interleave two
            // conversations in one file and make the parent unresumable.
            if committed.session != id {
                return;
            }
            let store = store.clone();
            let id = id.clone();
            let logged = committed.logged();
            // Fire-and-forget: a slow disk must not stall the turn, and a
            // failed write is reported, never fatal to the conversation.
            tokio::spawn(async move {
                if let Err(e) = store.append(&id, std::slice::from_ref(&logged)).await {
                    eprintln!("session-persistence: {e}");
                }
            });
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
