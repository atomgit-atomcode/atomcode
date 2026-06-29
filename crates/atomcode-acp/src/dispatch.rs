//! Session table and `session/new` handler.
//!
//! Owns the shared [`Sessions`] map and the monotone session counter.  The
//! handler is wired into the ACP builder in [`crate::serve_stdio`]; Tasks 7-9
//! add their own handlers that share the same table and counter.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{NewSessionRequest, NewSessionResponse, SessionId};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::provider::LlmProvider;
use tokio::sync::Mutex;

use crate::engine::EngineConfig;

// ── Session table ─────────────────────────────────────────────────────────────

/// Per-session state held in the shared table.
pub struct SessionState {
    pub handle: AgentHandle,
}

/// The shared session table: `session_id string → state`.
pub type Sessions = Arc<Mutex<HashMap<String, SessionState>>>;

// ── ID helper ─────────────────────────────────────────────────────────────────

/// Generate the ACP [`SessionId`] for sequence number `n`.
///
/// The format is `"acp-{n}"` — stable and unique as long as the counter is
/// monotone (which the `fetch_add` in [`handle_new_session`] guarantees).
pub fn new_session_id(n: u64) -> SessionId {
    SessionId::new(format!("acp-{n}"))
}

// ── session/new handler ───────────────────────────────────────────────────────

/// Handle a `session/new` request.
///
/// Spawns a kernel session, inserts it into the shared table, and returns the
/// fresh [`SessionId`] to the client.
///
/// `provider` — when `Some`, the pre-built (authenticated) provider is used
/// directly; when `None`, [`crate::engine::build_provider`] constructs a
/// fallback from the engine config (valid for non-gateway endpoints only).
pub async fn handle_new_session(
    engine: &EngineConfig,
    provider: Option<Arc<dyn LlmProvider>>,
    sessions: &Sessions,
    counter: &std::sync::atomic::AtomicU64,
    req: NewSessionRequest,
) -> Result<NewSessionResponse, agent_client_protocol::Error> {
    let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let id = new_session_id(n);
    // provider is the CLI-built, authenticated provider (Task 10); cloned per
    // session (Arc clone is cheap). engine::spawn_session falls back to its own
    // build_provider only when None (non-gateway test/dev paths).
    let handle = crate::engine::spawn_session(engine, req.cwd.clone(), provider)
        .await
        .map_err(|e| agent_client_protocol::util::internal_error(format!("{e}")))?;
    sessions
        .lock()
        .await
        .insert(id.0.to_string(), SessionState { handle });
    Ok(NewSessionResponse::new(id))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_id_is_stable_and_unique() {
        assert_eq!(new_session_id(1).0.as_ref(), "acp-1");
        assert_ne!(new_session_id(1).0.as_ref(), new_session_id(2).0.as_ref());
    }
}
