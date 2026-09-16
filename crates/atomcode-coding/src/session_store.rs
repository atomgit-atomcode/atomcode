//! `session-store`: the session's log, kept in the session store.
//!
//! A session's one authority is its event log (`docs/adr/0024`), and the store
//! that keeps it is the product's `SessionManager` — the same project buckets,
//! catalog, leases, rename and delete sessions always had. This row is the
//! `session-persistence` seam over it, for the one session the runtime opened
//! and holds the lease on.
//!
//! Every committed fact of that session is appended as it is committed, inline,
//! before the commit returns: an agent rebuilt a moment later replays the log,
//! and a fact still in a queue would be a fact it never sees. Streamed chunks are
//! the store's to skip.
//!
//! A write that fails stops the session. A log with a hole in it replays into a
//! conversation that never happened, so the first failure is final: nothing
//! after it is appended, the turn in flight is cancelled, and the runtime is
//! told — it fails closed when the turn ends, the way it does for every other
//! uncertain commit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::session::snapshot::SnapshotPersistenceStatus;
use atomcode_capabilities::session::{SessionLease, SessionManager};
use atomcode_harness::events::SessionEventCommitted;
use atomcode_harness::seams::{AgentsSvc, SessionPersistence, SessionPersistenceSvc};
use atomcode_harness::session::{Committed, LoggedEvent, SessionHeader};
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

/// The session the runtime opened, and what writing it needs.
#[derive(Clone)]
pub struct StoredSession {
    pub store: Arc<SessionManager>,
    pub lease: SessionLease,
    /// Where a failed write is reported, so the runtime stops.
    pub status: Option<SnapshotPersistenceStatus>,
}

struct Store {
    session: StoredSession,
    /// Set by the first write that failed. Nothing is appended after it.
    broken: AtomicBool,
}

impl Store {
    fn id(&self) -> &str {
        self.session.lease.id()
    }

    fn holds(&self, session_id: &str) -> bool {
        session_id == self.id()
    }

    fn append_now(&self, events: &[LoggedEvent]) -> Result<(), String> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(format!(
                "session {}: an earlier write failed; nothing more is kept",
                self.id()
            ));
        }
        self.session
            .store
            .append_events(&self.session.lease, events)
            .map_err(|error| {
                self.broken.store(true, Ordering::SeqCst);
                let message = format!("session {}: writing its log failed: {error}", self.id());
                if let Some(status) = &self.session.status {
                    status.report_uncertain_commit(message.clone());
                }
                message
            })
    }
}

#[async_trait]
impl SessionPersistence for Store {
    fn location(&self, session_id: &str) -> Option<String> {
        self.session
            .store
            .events_path(session_id)
            .ok()
            .map(|path| path.display().to_string())
    }

    /// The runtime created the session's log, header and all, when it opened
    /// it; an agent that exists before the session is published writes nothing.
    async fn begin(&self, _header: &SessionHeader) -> Result<(), String> {
        Ok(())
    }

    async fn header(&self, session_id: &str) -> Result<Option<SessionHeader>, String> {
        if !self.session.store.is_event_session(session_id) {
            return Ok(None);
        }
        self.session
            .store
            .read_event_header(session_id)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    async fn append(&self, session_id: &str, events: &[LoggedEvent]) -> Result<(), String> {
        if !self.holds(session_id) {
            return Err(format!(
                "session {session_id}: this runtime holds the lease on {} only",
                self.id()
            ));
        }
        self.append_now(events)
    }

    /// A session not published yet has nothing stored. One that has an index
    /// and no log is half a session, and says so.
    async fn load(&self, session_id: &str) -> Result<Vec<LoggedEvent>, String> {
        if !self.session.store.is_event_session(session_id) {
            return Ok(Vec::new());
        }
        self.session
            .store
            .load_events(session_id)
            .map_err(|error| error.to_string())
    }

    async fn list(&self) -> Result<Vec<String>, String> {
        let mut sessions = self.session.store.list();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions
            .into_iter()
            .map(|meta| meta.id)
            .filter(|id| self.session.store.is_event_session(id))
            .collect())
    }
}

/// See the module docs.
pub(crate) struct SessionStorePlugin(pub(crate) StoredSession);

#[async_trait]
impl Plugin for SessionStorePlugin {
    fn name(&self) -> &'static str {
        "session-store"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["agents"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-persistence"]
    }
    fn description(&self) -> &'static str {
        "append the runtime's session's committed facts to its log in the session store"
    }

    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let store = Arc::new(Store {
            session: self.0.clone(),
            broken: AtomicBool::new(false),
        });
        let _ = ctx
            .provide::<SessionPersistenceSvc>(store.clone())
            .map_err(|e| e.to_string())?;

        let agents_ctx = ctx.clone();
        let _ = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            if !store.holds(&committed.session) {
                return;
            }
            let Some(agent) = agents_ctx
                .service::<AgentsSvc>()
                .and_then(|agents| agents.by_session(&committed.session))
            else {
                return;
            };
            if !agent.persist() {
                return;
            }
            let was_broken = store.broken.load(Ordering::SeqCst);
            if store
                .append_now(std::slice::from_ref(&committed.logged()))
                .is_err()
            {
                if !was_broken {
                    eprintln!(
                        "session-store: session {}: a write failed; stopping",
                        store.id()
                    );
                }
                agent.cancel();
            }
        });
        Ok(())
    }
}
