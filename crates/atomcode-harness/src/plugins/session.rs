//! The session domain: the log, the projection registry, and a durable store.
//!
//! Three rows rather than one, because they are three decisions. A deployment
//! can keep the log and drop persistence (an eval run), or keep persistence and
//! swap the store (a database instead of JSONL), without any of them knowing.

mod jsonl;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use async_trait::async_trait;
use atomcode_config::i18n::{t, Msg};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::{Agent, AgentId};
use crate::events::{AgentChange, AgentCreated, AgentInfo, AgentRemoved, SessionEventCommitted};
use crate::seams::{
    AgentsSvc, SessionDefaults, SessionDefaultsSvc, SessionPersistence, SessionPersistenceSvc,
    SessionProjectionsSvc,
};
use crate::session::{Committed, LoggedEvent, ProjectionUnit, SessionEvent, SessionProjections};

use self::jsonl::{JsonlStore, LiveJournal};

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

struct JournalWriters {
    active: HashMap<String, (AgentId, Arc<LiveJournal>)>,
    queue: Option<mpsc::Sender<(Arc<LiveJournal>, LoggedEvent)>>,
}

impl JournalWriters {
    fn keep(&mut self, store: &JsonlStore, agent: &Agent) -> Result<(), String> {
        let id = agent.session_id();
        if self.queue.is_none()
            || !agent.persist()
            || self
                .active
                .get(id)
                .is_some_and(|(owner, _)| *owner == agent.id())
        {
            return Ok(());
        }
        let journal = store.open_live(agent.session().header())?;
        self.active
            .insert(id.to_string(), (agent.id(), Arc::new(journal)));
        Ok(())
    }
}

pub struct SessionPersistenceJsonlPlugin;

#[async_trait]
impl Plugin for SessionPersistenceJsonlPlugin {
    fn name(&self) -> &'static str {
        "session-persistence-jsonl"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["product-dirs"]
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
        let root =
            row.root.map(PathBuf::from).map(Ok).unwrap_or_else(|| {
                crate::product_dirs(ctx).map(|dirs| dirs.user().join("sessions"))
            })?;
        let project = row
            .project_root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        // The same bucket id the rest of the stack uses, so the two session
        // stores under this root stop sharing one level and start agreeing on
        // what a project is.
        let bucket = atomcode_config::util::stable_project_hash(&project);
        let store = Arc::new(JsonlStore::new(root, bucket));
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
                    .service::<AgentsSvc>()
                    .and_then(|agents| agents.by_session(log.id()))
                    .map(|agent| agent.persist())
                    .unwrap_or(true);
                if !persisted {
                    return None;
                }
                let mut lines = vec![format!("event log: {}", facts_store.location(log.id())?)];
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
        // A single off-turn writer keeps commit order. The fiber drains it
        // before releasing leases on shutdown so an immediate resume sees the
        // complete persisted prefix, including the last queued turn facts.
        let (writes, queue) = mpsc::channel::<(Arc<LiveJournal>, LoggedEvent)>();
        let worker = std::thread::Builder::new()
            .name("harness-session-jsonl".into())
            .spawn(move || {
                for (journal, logged) in queue {
                    journal.commit(&logged);
                }
            })
            .map_err(|e| {
                t(Msg::HarnessJournalWriterStartFailed {
                    error: &e.to_string(),
                })
                .into_owned()
            })?;
        let writers = Arc::new(Mutex::new(JournalWriters {
            active: HashMap::new(),
            queue: Some(writes),
        }));
        let stopping = writers.clone();
        let _ = ctx.effect(move || {
            {
                let mut writers = stopping.lock().expect("journal registry poisoned");
                writers.queue.take();
                writers.active.clear();
            }
            if worker.join().is_err() {
                eprintln!(
                    "session-persistence: {}",
                    t(Msg::HarnessJournalWriterTerminated)
                );
            }
        });

        let created_writers = writers.clone();
        let header_ctx = ctx.clone();
        let header_store = store.clone();
        let _ = ctx.on_emit::<AgentCreated>(move |created: &AgentInfo| {
            let Some(agent) = header_ctx
                .service::<AgentsSvc>()
                .and_then(|a| a.get(created.id))
            else {
                return;
            };
            if let Err(e) = created_writers
                .lock()
                .expect("journal registry poisoned")
                .keep(&header_store, &agent)
            {
                agent.reject_creation(format!("session-persistence: {e}"));
            }
        });
        let removed_writers = writers.clone();
        let _ = ctx.on_emit::<AgentRemoved>(move |removed: &AgentChange| {
            let mut writers = removed_writers.lock().expect("journal registry poisoned");
            if writers
                .active
                .get(&removed.session)
                .is_some_and(|(id, _)| *id == removed.id)
            {
                writers.active.remove(&removed.session);
            }
        });

        // A config patch can mount this row after agents already exist.
        if let Some(agents) = ctx.service::<AgentsSvc>() {
            let mut active = writers.lock().expect("journal registry poisoned");
            for agent in agents.list() {
                active.keep(&store, &agent)?;
            }
        }

        let _ = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            let writers = writers.lock().expect("journal registry poisoned");
            if let (Some(queue), Some((_, journal))) =
                (&writers.queue, writers.active.get(&committed.session))
            {
                // Pending facts retain the lease even after agent removal.
                let _ = queue.send((journal.clone(), committed.logged()));
            }
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
