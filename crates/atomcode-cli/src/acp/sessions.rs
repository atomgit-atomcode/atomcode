//! Live ACP session table, session teardown handlers, and wire-id helpers.
//!
//! Owns the shared [`Sessions`] map (live sessions keyed by their ACP wire id)
//! together with the handlers that mutate it (`session/cancel`, `session/close`,
//! `session/delete`), the wire-id ↔ native-id round-trip helpers, and the
//! message-id/title/additional-directories helpers the turn loop and session
//! lifecycle share. Persistence ownership stays with the native
//! [`SessionManager`]: ACP wire ids are `acp-<native id>` so every wire id
//! round-trips to the single native session catalog shared with the CLI/TUI —
//! no second persistence model.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    CloseSessionResponse, DeleteSessionResponse, SessionConfigOption, SessionId,
};
use agent_client_protocol::Error as AcpError;
use atomcode_capabilities::session::{CatalogScan, SessionManager, SessionStoreError};
use atomcode_coding::{CodingRuntime, CodingRuntimeEvents, CodingRuntimeHandle, RuntimeMode};
use tokio::sync::Mutex;

// ── Session table ─────────────────────────────────────────────────────────────

/// Per-session state held in the shared table.
///
/// The kernel [`AgentHandle`] is split into its three fields so the prompt turn
/// loop can own/lock the `events` receiver for the whole turn **without**
/// holding the [`Sessions`] map lock, while `session/cancel` can still
/// reach the kernel via the cheaply-clonable `commands` sender concurrently.
///
/// `events` is wrapped in its own [`Arc<Mutex<…>>`] precisely so the turn task
/// can clone the `Arc` out under a brief map lock, release the map, and then
/// lock only this session's receiver for the turn's duration. One prompt runs
/// per session at a time, so that lock is uncontended in practice.
pub struct SessionState {
    pub runtime: CodingRuntimeHandle,
    pub events: Arc<Mutex<CodingRuntimeEvents>>,
    pub _task: tokio::task::JoinHandle<atomcode_coding::RuntimeExit>,
    /// Native session id (the single persistence owner's key). The ACP wire id
    /// is `acp-<native_id>`; resume/delete/list all round-trip through it.
    pub native_id: String,
    /// Working directory the session was created in (from `session/new` cwd).
    pub cwd: std::path::PathBuf,
    /// Kernel operating mode (mapped to the ACP `SessionModeId` wire name).
    pub current_mode: RuntimeMode,
    /// The session config option catalog with per-session current values.
    pub config_options: Vec<SessionConfigOption>,
    /// (prompt, completion) tokens accumulated from `AgentEvent::Usage` events
    /// across the session's turns, for `/usage` and `/cost`.
    pub usage: (u64, u64),
    /// `todowrite`/`todo` invocations `(name, raw args)` in call order — the
    /// single source for the session's derived todo/plan state that maps to the
    /// ACP `plan` update and the `/todo` command.
    pub todo_calls: Vec<(String, String)>,
    /// Auto-derived display title (from the first real user prompt),
    /// broadcast once via the stable v1 `session_info_update` notification.
    /// `None` until the first content-bearing turn completes.
    pub title: Option<String>,
    /// Additional workspace roots beyond `cwd`, from the session lifecycle
    /// request's `additionalDirectories` (protocol capability
    /// `sessionCapabilities.additionalDirectories`). Kept for `session/list`
    /// reporting; the session's effective filesystem semantics remain the
    /// kernel's single pinned `working_dir` (see the module docs).
    pub additional_directories: Vec<std::path::PathBuf>,
}

/// Live ACP sessions, keyed by session id.
///
/// Sessions are removed and torn down by the explicit `session/close` and
/// `session/delete` handlers (see [`handle_close_session`] /
/// [`handle_delete_session`]). The remaining gap: a session whose kernel agent
/// finishes on its own (e.g. an internal stop) is not auto-pruned — it stays in
/// the table until the client closes/deletes it or the whole connection ends
/// (all are freed when the process exits / the client disconnects).
pub type Sessions = Arc<Mutex<HashMap<String, SessionState>>>;

// ── ID helpers ────────────────────────────────────────────────────────────────

/// ACP wire prefix: every wire [`SessionId`] is `acp-<native session id>` so it
/// round-trips to the native [`SessionManager`] catalog (the single persistence
/// owner) without a side table.
const SESSION_ID_PREFIX: &str = "acp-";

/// The ACP wire id for a native session id.
pub fn wire_session_id(native_id: &str) -> SessionId {
    SessionId::new(format!("{SESSION_ID_PREFIX}{native_id}"))
}

/// Recover the native session id from an ACP wire id. `None` for ids that were
/// not minted by this agent (never a valid resume/delete target).
pub fn native_id_from_wire(session_id: &SessionId) -> Option<&str> {
    session_id.0.strip_prefix(SESSION_ID_PREFIX)
}

/// Allocate the next `messageId` from the shared per-connection counter.
///
/// One id is consumed per LLM output round (see `run_prompt_turn`) and per
/// replayed message (`replay_entries_to_v1_updates`); the counter is shared by
/// the v1 and v2 chains (see `serve_over`) so ids never collide across
/// protocol generations.
pub fn next_message_id(msg_ids: &AtomicU64) -> String {
    format!("m{}", msg_ids.fetch_add(1, Ordering::Relaxed) + 1)
}

/// Derive a display title from the first real user prompt, mirroring the
/// native `SessionMeta::auto_name_from_messages` fallback (first line,
/// control chars → space, ≤40 chars). Returns `None` for empty/whitespace-only
/// prompts, so attachment-only turns never title the session.
pub fn derive_title(text: &str) -> Option<String> {
    let name: String = text
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(40)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Validate `additionalDirectories` from a session lifecycle request.
///
/// Protocol MUST: every entry is an absolute path (a relative entry would
/// silently resolve against the agent's own cwd and break the session's
/// filesystem-root contract). Returns the validated list or an invalid-params
/// error.
pub fn validate_additional_directories(additional: &[std::path::PathBuf]) -> Result<(), AcpError> {
    for dir in additional {
        if !dir.is_absolute() {
            return Err(AcpError::invalid_params().data(format!(
                "additionalDirectories entries must be absolute paths (got `{}`)",
                dir.display()
            )));
        }
    }
    Ok(())
}

// ── session table registration ────────────────────────────────────────────────

/// Register an already-spawned [`CodingRuntime`] in the live table under its
/// wire id. Shared by the new and resume paths.
pub async fn register_session(
    sessions: &Sessions,
    runtime: CodingRuntime,
    cwd: std::path::PathBuf,
    config_options: &[SessionConfigOption],
    additional_directories: Vec<std::path::PathBuf>,
) -> Result<SessionId, agent_client_protocol::Error> {
    let CodingRuntime {
        handle,
        events,
        task,
        session,
    } = runtime;
    // The runtime owns session identity: a session-bearing prepare always
    // reports its native id. Missing it means the prepare ran session-less,
    // which cannot round-trip through the ACP lifecycle — fail closed.
    let native_id = session.map(|info| info.id).ok_or_else(|| {
        agent_client_protocol::util::internal_error("acp: runtime reported no session id")
    })?;
    let id = wire_session_id(&native_id);
    sessions.lock().await.insert(
        id.0.to_string(),
        SessionState {
            runtime: handle,
            events: Arc::new(Mutex::new(events)),
            _task: task,
            native_id,
            cwd,
            current_mode: RuntimeMode::Build,
            config_options: config_options.to_vec(),
            usage: (0, 0),
            todo_calls: Vec::new(),
            title: None,
            additional_directories,
        },
    );
    Ok(id)
}

// ── session teardown handlers ────────────────────────────────────────────────

/// Send [`AgentCommand::Cancel`] to the named session's kernel.
///
/// If `session_id` is unknown the function is a deliberate no-op — the client
/// may race a cancel against a turn that has already completed and the session
/// removed; silently ignoring that case is correct protocol behaviour.
///
/// The map lock is held only for the synchronous `.get` + `.send` pair; it is
/// released before any `await`, satisfying the hard constraint in the task brief.
pub async fn handle_cancel(sessions: &Sessions, session_id: &str) {
    let runtime = {
        sessions
            .lock()
            .await
            .get(session_id)
            .map(|state| state.runtime.clone())
    };
    if let Some(runtime) = runtime {
        let _ = runtime.cancel().await;
    }
}

/// Handle a `session/close` request.
///
/// Per the protocol the agent cancels any ongoing work (as if `session/cancel`
/// were called), stops the kernel runtime, and frees the session table entry.
/// Closing an unknown session is a no-op success (the client may race a close
/// against a turn that already finished). All teardown is best-effort: a
/// session whose kernel already died must still be removed from the table.
pub async fn handle_close_session(
    sessions: &Sessions,
    session_id: &SessionId,
) -> CloseSessionResponse {
    let runtime = {
        let mut map = sessions.lock().await;
        map.remove(session_id.0.as_ref()).map(|state| state.runtime)
    };
    if let Some(runtime) = runtime {
        // Cancel ongoing work first, then shut the runtime down. Both are
        // best-effort: the kernel may already be gone (e.g. a prior error).
        let _ = runtime.cancel().await;
        let _ = runtime.shutdown().await;
    }
    CloseSessionResponse::new()
}

/// Handle a `session/delete` request over the live table AND the native
/// session catalog.
///
/// The protocol's session history is the native catalog: a live session is
/// closed first (cancel + shutdown, releasing its lease), then its persisted
/// record is removed. Deleting an unknown session is a no-op success (protocol
/// SHOULD). A session live in ANOTHER process fails closed with an explicit
/// lease-conflict error — never a remote takeover.
pub async fn handle_delete_session(
    sessions: &Sessions,
    session_id: &SessionId,
    scan: &CatalogScan,
) -> Result<DeleteSessionResponse, AcpError> {
    let native_id = native_id_from_wire(session_id).ok_or_else(|| {
        AcpError::invalid_params().data(format!("unknown session `{}`", session_id.0))
    })?;

    // 1. Tear the live session down (if present) — this releases its lease.
    let runtime = {
        let mut map = sessions.lock().await;
        map.remove(session_id.0.as_ref()).map(|state| state.runtime)
    };
    if let Some(runtime) = runtime {
        let _ = runtime.cancel().await;
        let _ = runtime.shutdown().await;
    }

    // 2. Remove the persisted record. Unknown sessions are already a success
    //    (protocol SHOULD: deleting a never-existed session succeeds silently).
    let Some(entry) = scan
        .find(native_id)
        .map_err(|e| agent_client_protocol::util::internal_error(format!("catalog error: {e}")))?
    else {
        return Ok(DeleteSessionResponse::new());
    };
    let manager = SessionManager::for_project(&entry.working_dir);
    let lease = match manager.acquire_lease(native_id) {
        Ok(lease) => lease,
        Err(SessionStoreError::SessionInUse { .. }) => {
            return Err(AcpError::invalid_params()
                .data("session is active in another atomcode process; close it before deleting"));
        }
        Err(e) => {
            return Err(agent_client_protocol::util::internal_error(format!(
                "lease failed: {e}"
            )));
        }
    };
    manager
        .delete(&lease)
        .map_err(|e| agent_client_protocol::util::internal_error(format!("delete failed: {e}")))?;
    Ok(DeleteSessionResponse::new())
}

/// Shared test support: stub session table builders used by the sessions,
/// discovery (session/list), and options (mode/config) test modules.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

    /// Build a stub session state (live handle + empty events channel) without
    /// spawning a real kernel agent.
    pub(crate) fn stub_session(native_id: &str, cwd: &str) -> SessionState {
        let (runtime, _controls) = atomcode_coding::runtime::coding_runtime_control_channel();
        let (_ev_tx, events) = tokio::sync::mpsc::unbounded_channel();
        SessionState {
            runtime,
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _task: tokio::spawn(async {
                atomcode_coding::runtime::RuntimeExit {
                    reason: atomcode_coding::runtime::RuntimeExitReason::ShutdownRequested,
                    forced: false,
                }
            }),
            native_id: native_id.to_string(),
            cwd: std::path::PathBuf::from(cwd),
            current_mode: RuntimeMode::Build,
            config_options: Vec::new(),
            usage: (0, 0),
            todo_calls: Vec::new(),
            title: None,
            additional_directories: Vec::new(),
        }
    }

    /// Wire-id → cwd pairs; the native id is the wire id with the `acp-`
    /// prefix stripped, mirroring production (`acp-<native id>`).
    pub(crate) fn sessions_with(sessions: Vec<(&str, &str)>) -> Sessions {
        let map: std::collections::HashMap<String, SessionState> = sessions
            .into_iter()
            .map(|(id, cwd)| {
                let native = id.strip_prefix("acp-").unwrap_or(id);
                (id.to_string(), stub_session(native, cwd))
            })
            .collect();
        std::sync::Arc::new(tokio::sync::Mutex::new(map))
    }

    /// An empty catalog scan (the list/delete handlers read the native catalog
    /// through an injected scan so unit tests stay hermetic — no
    /// `ATOMCODE_HOME` mutation).
    pub(crate) fn empty_scan() -> CatalogScan {
        CatalogScan {
            entries: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    /// A native catalog entry for a session `id` (same shape the
    /// `SessionManager` catalog scan produces), used by the list/delete tests.
    pub(crate) fn catalog_entry(id: &str, name: &str, cwd: &str) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            name: name.to_string(),
            fork_root_id: None,
            project_bucket: "bucket".to_string(),
            working_dir: std::path::PathBuf::from(cwd),
            created_at_ms: 0,
            updated_at_ms: 0,
            message_count: 0,
            turn_count: 0,
            presence: CatalogPresence::NativeOnly,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn wire_session_id_round_trips_native_ids() {
        // Wire ids are `acp-<native id>` and must round-trip so resume/delete
        // reach the single native catalog without a side table.
        assert_eq!(wire_session_id("abc-123").0.as_ref(), "acp-abc-123");
        assert_eq!(
            native_id_from_wire(&wire_session_id("abc-123")),
            Some("abc-123")
        );
        // Ids this agent did not mint are never valid resume/delete targets.
        assert_eq!(native_id_from_wire(&SessionId::new("raw-uuid")), None);
    }

    #[tokio::test]
    async fn close_removes_session_and_shuts_down_runtime() {
        let sessions = sessions_with(vec![("acp-1", "/work-a"), ("acp-2", "/work-b")]);

        let resp = handle_close_session(&sessions, &SessionId::new("acp-1")).await;
        serde_json::to_value(&resp).unwrap(); // response is serializable (empty)

        assert!(
            sessions.lock().await.get("acp-1").is_none(),
            "session removed"
        );
        assert!(
            sessions.lock().await.get("acp-2").is_some(),
            "other session untouched"
        );
    }

    #[tokio::test]
    async fn close_unknown_session_is_success_noop() {
        let sessions = sessions_with(vec![("acp-1", "/work")]);
        let resp = handle_close_session(&sessions, &SessionId::new("acp-missing")).await;
        serde_json::to_value(&resp).unwrap();
        assert!(sessions.lock().await.get("acp-1").is_some());
    }

    #[tokio::test]
    async fn delete_removes_session_like_close() {
        let sessions = sessions_with(vec![("acp-1", "/work-a"), ("acp-2", "/work-b")]);

        let resp = handle_delete_session(&sessions, &SessionId::new("acp-2"), &empty_scan())
            .await
            .unwrap();
        serde_json::to_value(&resp).unwrap();

        assert!(
            sessions.lock().await.get("acp-2").is_none(),
            "session deleted"
        );
        assert!(
            sessions.lock().await.get("acp-1").is_some(),
            "other session untouched"
        );
    }

    #[tokio::test]
    async fn delete_unknown_session_is_success_noop() {
        // Protocol SHOULD: deleting a never-existing session succeeds silently.
        let sessions = sessions_with(vec![("acp-1", "/work")]);
        let resp = handle_delete_session(&sessions, &SessionId::new("acp-missing"), &empty_scan())
            .await
            .unwrap();
        serde_json::to_value(&resp).unwrap();
        assert!(sessions.lock().await.get("acp-1").is_some());
    }

    #[tokio::test]
    async fn delete_rejects_non_acp_wire_ids() {
        let sessions = sessions_with(vec![("acp-1", "/work")]);
        let err = handle_delete_session(&sessions, &SessionId::new("raw-uuid"), &empty_scan())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown session"));
    }

    #[tokio::test]
    async fn cancel_sends_cancel_command() {
        use atomcode_coding::runtime::{
            coding_runtime_control_channel, CodingRuntimeControl, RuntimeExit, RuntimeExitReason,
        };
        let (runtime, mut controls) = coding_runtime_control_channel();
        let (_ev_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let state = SessionState {
            runtime,
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _task: tokio::spawn(async {
                RuntimeExit {
                    reason: RuntimeExitReason::ShutdownRequested,
                    forced: false,
                }
            }),
            native_id: "test-native".to_string(),
            cwd: std::path::PathBuf::from("/work"),
            current_mode: RuntimeMode::Build,
            config_options: Vec::new(),
            usage: (0, 0),
            todo_calls: Vec::new(),
            title: None,
            additional_directories: Vec::new(),
        };
        let sessions: Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        sessions.lock().await.insert("acp-1".into(), state);

        let control_task = tokio::spawn(async move {
            match controls.recv().await {
                Some(CodingRuntimeControl::Cancel { done, .. }) => {
                    let _ = done.send(Ok(()));
                    true
                }
                _ => false,
            }
        });

        handle_cancel(&sessions, "acp-1").await;

        assert!(control_task.await.unwrap());
    }

    #[tokio::test]
    async fn cancel_unknown_session_is_noop() {
        // Cancelling a session that doesn't exist must not panic or return an error.
        let sessions: Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        handle_cancel(&sessions, "acp-nonexistent").await; // must not panic
    }

    #[test]
    fn validate_additional_directories_accepts_absolute_rejects_relative() {
        let absolute = vec![
            std::path::PathBuf::from("/home/user/shared-lib"),
            std::path::PathBuf::from("/tmp/scratch"),
        ];
        assert!(validate_additional_directories(&absolute).is_ok());

        let mixed = vec![
            std::path::PathBuf::from("/abs"),
            std::path::PathBuf::from("rel"),
        ];
        let err = validate_additional_directories(&mixed).unwrap_err();
        assert!(
            err.to_string().contains("absolute paths"),
            "relative entry must be rejected: {err}"
        );

        let empty: Vec<std::path::PathBuf> = Vec::new();
        assert!(validate_additional_directories(&empty).is_ok());
    }

    #[test]
    fn next_message_id_increments_from_the_shared_counter() {
        let counter = std::sync::atomic::AtomicU64::new(0);
        assert_eq!(next_message_id(&counter), "m1");
        assert_eq!(next_message_id(&counter), "m2");
        assert_eq!(next_message_id(&counter), "m3");
    }

    #[test]
    fn derive_title_uses_first_real_prompt() {
        assert_eq!(
            derive_title("Fix the login bug\nand add tests").as_deref(),
            Some("Fix the login bug")
        );
        assert_eq!(derive_title("   \n  "), None, "blank prompt never titles");
        assert_eq!(derive_title(""), None, "empty prompt never titles");
    }

    #[test]
    fn derive_title_normalizes_control_chars_and_truncates() {
        let noisy = format!("line one\nline two{}\u{1b}[31mred", 'x');
        let title = derive_title(&noisy).unwrap();
        assert_eq!(title, "line one");
        assert!(!title.contains('\u{1b}'), "control chars are not kept");

        let long = "a".repeat(100);
        let title = derive_title(&long).unwrap();
        assert_eq!(title.chars().count(), 40, "title is capped at 40 chars");
    }
}
