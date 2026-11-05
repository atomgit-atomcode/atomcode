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
//!
//! What the session delegates is kept too (`docs/adr/0024` §11, §12): a team
//! member's and a `task` child's log is a session of its own in the same
//! bucket, stored under [`storage_id`] with its parent in its index — so no
//! catalog lists it — and written under a lease of its own, taken when the
//! agent is created and let go when it is removed. One whose log cannot be kept
//! does not run: its creation is refused. A failed write stops that agent, not
//! the session it was delegated from.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_capabilities::session::events::storage_id;
use atomcode_capabilities::session::snapshot::SnapshotPersistenceStatus;
use atomcode_capabilities::session::{SessionLease, SessionManager, SessionMeta, StorageOwner};
use atomcode_harness::agent::Agent;
use atomcode_harness::events::{
    AgentChange, AgentCreated, AgentInfo, AgentRemoved, SessionEventCommitted,
};
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
    /// The delegated agents' sessions this runtime keeps, by session id.
    delegated: Mutex<HashMap<String, Delegated>>,
}

struct Delegated {
    agent_id: atomcode_harness::agent::AgentId,
    lease: SessionLease,
    /// Set by the first write that failed, as for the session's own.
    broken: bool,
}

impl Store {
    fn id(&self) -> &str {
        self.session.lease.id()
    }

    fn holds(&self, session_id: &str) -> bool {
        session_id == self.id()
    }

    /// Take the lease on a delegated agent's session and create its log, unless
    /// it is being resumed from the one it has.
    fn keep(&self, agent: &Agent) -> Result<(), String> {
        let store = &self.session.store;
        let header = agent.session().header().clone();
        let id = storage_id(&header.id);
        let lease = store.acquire_lease(&id).map_err(|e| {
            let held_here = self.delegated.lock().expect("delegated sessions poisoned")
                .contains_key(&header.id);
            tracing::warn!(runtime_session = self.id(), member_session = %header.id,
                storage_id = %id, agent_id = agent.id(), process_id = std::process::id(),
                held_by_this_store = held_here, error = %e, "member session lease acquisition failed");
            format!("{e} (pid={}, parent={}, member={}, held_by_this_store={held_here})",
                std::process::id(), self.id(), header.id)
        })?;
        tracing::debug!(runtime_session = self.id(), member_session = %header.id,
            agent_id = agent.id(), "member session lease acquired");
        if !store.is_event_session(&id) {
            let working_dir = store
                .read_meta(self.id())
                .map(|meta| meta.working_dir)
                .ok()
                .or_else(|| header.cwd.clone())
                .unwrap_or_default();
            let created = i64::try_from(header.created_at).unwrap_or(i64::MAX);
            let mut meta = SessionMeta::new(&id, working_dir, created);
            meta.owner = StorageOwner::Native;
            meta.parent = header.parent.clone();
            if let Some(member) = &header.member {
                meta.name = member.name.clone();
            }
            store
                .create_event_session(&lease, &header, &meta)
                .map_err(|e| e.to_string())?;
        } else if let Some(mut summary) = store
            .recover_interrupted_turn(&lease)
            .map_err(|e| e.to_string())?
        {
            // AgentCreated runs before a restored member can execute. Its
            // lease has just been acquired; refresh the seed from the log so
            // the next commit uses the sequence after the recovery facts.
            let events = store.load_events(&id).map_err(|e| e.to_string())?;
            let messages = atomcode_kernel::session::derive_messages_with_meta(&events);
            if let Some(tasks) = crate::todo::recovery_task_summary(&messages, store, &id) {
                summary.push('\n');
                summary.push_str(&tasks);
            }
            agent.session().restore(events);
            self.warn(format!("{}: {summary}", header.id));
        }
        self.delegated
            .lock()
            .expect("delegated sessions poisoned")
            .insert(
                header.id,
                Delegated {
                    agent_id: agent.id(),
                    lease,
                    broken: false,
                },
            );
        Ok(())
    }

    fn release(&self, removed: &AgentChange) {
        let mut held = self.delegated.lock().expect("delegated sessions poisoned");
        if held
            .get(&removed.session)
            .is_some_and(|kept| kept.agent_id == removed.id)
        {
            held.remove(&removed.session);
            tracing::debug!(runtime_session = self.id(), member_session = %removed.session,
                agent_id = removed.id, "member removed; stored session lease released");
        }
    }

    /// Append to a delegated agent's session under its lease. `None` when this
    /// runtime keeps no such session.
    fn append_delegated(
        &self,
        session_id: &str,
        events: &[LoggedEvent],
    ) -> Option<Result<(), String>> {
        let mut delegated = self.delegated.lock().expect("delegated sessions poisoned");
        let kept = delegated.get_mut(session_id)?;
        if kept.broken {
            return Some(Err(format!(
                "session {session_id}: an earlier write failed; nothing more is kept"
            )));
        }
        Some(
            self.session
                .store
                .append_events(&kept.lease, events)
                .map_err(|error| {
                    kept.broken = true;
                    format!("session {session_id}: writing its log failed: {error}")
                }),
        )
    }

    /// Tell the person something about a session this store could not keep —
    /// a member's log, say — through the runtime's warning channel, which every
    /// front end renders. Not stderr: a full-screen front end is holding it.
    fn warn(&self, message: String) {
        if let Some(status) = &self.session.status {
            status.report_auxiliary_warning(format!("session-store: {message}"));
        }
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
            .events_path(&storage_id(session_id))
            .ok()
            .map(|path| path.display().to_string())
    }

    /// The runtime created the session's log, header and all, when it opened
    /// it; an agent that exists before the session is published writes nothing.
    async fn begin(&self, _header: &SessionHeader) -> Result<(), String> {
        Ok(())
    }

    async fn header(&self, session_id: &str) -> Result<Option<SessionHeader>, String> {
        let session_id = &storage_id(session_id);
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
        if self.holds(session_id) {
            return self.append_now(events);
        }
        self.append_delegated(session_id, events)
            .unwrap_or_else(|| {
                Err(format!(
                    "session {session_id}: this runtime holds no lease on it"
                ))
            })
    }

    /// A session not published yet has nothing stored. One that has an index
    /// and no log is half a session, and says so.
    async fn load(&self, session_id: &str) -> Result<Vec<LoggedEvent>, String> {
        let session_id = &storage_id(session_id);
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

    async fn children(&self, parent: &str) -> Result<Vec<SessionHeader>, String> {
        let store = &self.session.store;
        Ok(store
            .children(parent)
            .into_iter()
            .filter(|meta| store.is_event_session(&meta.id))
            .filter_map(|meta| store.read_event_header(&meta.id).ok())
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
            delegated: Mutex::new(HashMap::new()),
        });
        let _ = ctx
            .provide::<SessionPersistenceSvc>(store.clone())
            .map_err(|e| e.to_string())?;

        // Inline, as for the session's own log: the header is on disk before the
        // agent's first fact, and the lease is held before it can run.
        let created_ctx = ctx.clone();
        let created_store = store.clone();
        let _ = ctx.on_emit::<AgentCreated>(move |created: &AgentInfo| {
            let Some(agent) = created_ctx
                .service::<AgentsSvc>()
                .and_then(|agents| agents.get(created.id))
            else {
                return;
            };
            if agent.parent().is_none() || !agent.persist() {
                return;
            }
            if let Err(e) = created_store.keep(&agent) {
                created_store.warn(format!(
                    "session {}: its log cannot be kept, so it does not run: {e}",
                    agent.session_id()
                ));
                agent.reject_creation(e);
            }
        });
        let removed_store = store.clone();
        let _ = ctx.on_emit::<AgentRemoved>(move |removed: &AgentChange| {
            removed_store.release(removed);
        });

        let agents_ctx = ctx.clone();
        let _ = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            let Some(agent) = agents_ctx
                .service::<AgentsSvc>()
                .and_then(|agents| agents.by_session(&committed.session))
            else {
                return;
            };
            if !agent.persist() {
                return;
            }
            if !store.holds(&committed.session) {
                if agent.parent().is_none() {
                    return;
                }
                let written = store.append_delegated(
                    &committed.session,
                    std::slice::from_ref(&committed.logged()),
                );
                match written {
                    Some(Ok(())) => {}
                    Some(Err(e)) => {
                        store.warn(format!("{e}; stopping it"));
                        agent.cancel();
                    }
                    // Its log could not be kept when it was created.
                    None => agent.cancel(),
                }
                return;
            }
            // A failed write is already reported as an uncertain commit by
            // `append_now`, which the runtime turns into its fail-closed stop.
            if store
                .append_now(std::slice::from_ref(&committed.logged()))
                .is_err()
            {
                agent.cancel();
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod lease_owner_tests {
    use super::*;

    #[test]
    fn removing_another_incarnation_cannot_release_the_current_members_lease() {
        let dir = tempfile::tempdir().unwrap();
        let dirs =
            atomcode_capabilities::ProductDirs::new(dir.path().join("state"), ".test-product");
        let manager = Arc::new(SessionManager::for_project(dir.path(), &dirs));
        let store = Store {
            session: StoredSession {
                lease: manager.acquire_lease("lead").unwrap(),
                store: manager.clone(),
                status: None,
            },
            broken: AtomicBool::new(false),
            delegated: Mutex::new(HashMap::from([(
                "lead/member".into(),
                Delegated {
                    agent_id: 2,
                    lease: manager.acquire_lease("lead~member").unwrap(),
                    broken: false,
                },
            )])),
        };
        let mut removed = AgentChange {
            id: 1,
            session: "lead/member".into(),
            parent: Some("lead".into()),
            status: atomcode_harness::agent::AgentStatus::Idle,
        };
        store.release(&removed);
        assert!(matches!(
            manager.acquire_lease("lead~member"),
            Err(atomcode_capabilities::session::SessionStoreError::SessionInUse { .. })
        ));
        removed.id = 2;
        store.release(&removed);
        assert!(manager.acquire_lease("lead~member").is_ok());
    }
}
