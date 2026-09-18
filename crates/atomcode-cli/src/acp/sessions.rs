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
use atomcode_coding::front_end::FrontEnd;
use atomcode_coding::{CodingAgentConfig, CodingRuntime, RuntimeMode};
use atomcode_host_api::{HostControl, HostEvent};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use tokio::sync::mpsc;
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
    /// What this session is driven with: the handle protocol
    /// (`docs/adr/0021` §1), the same one the full-screen UI uses.
    pub commands: mpsc::UnboundedSender<AgentCommand>,
    /// And what is asked of the host above it (§2).
    pub control: Arc<dyn HostControl>,
    /// The turn's facts, as the contract says them.
    pub events: Arc<Mutex<mpsc::UnboundedReceiver<AgentEvent>>>,
    /// Kept alive because the connection borrows it: it holds the session log
    /// the host reads to answer "is what you saw still current".
    pub _front_end: Arc<FrontEnd>,
    /// The last "the turn finished but its record did not", when there was one.
    ///
    /// Pushed by the host (`HostEvent::PersistenceFailed`) rather than read off
    /// the turn's terminal: the turn did complete, and whether it was written
    /// down is a separate fact about the store (see `docs/adr/0024` and the
    /// plan's 6.3 note). Read once at the end of a turn and cleared.
    pub persistence_failure: Arc<Mutex<Option<String>>>,
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

/// A host that says yes and remembers what it was asked.
///
/// For criteria about *what the screen asks of a host* — which is what the
/// option handlers do now that resolving a model is the host's job.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingHost {
    pub asked: std::sync::Mutex<Vec<atomcode_host_api::HostCommand>>,
    /// Answers, in order. Empty (or exhausted) answers `Done`.
    pub replies: std::sync::Mutex<std::collections::VecDeque<atomcode_host_api::HostReply>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl HostControl for RecordingHost {
    async fn call(
        &self,
        command: atomcode_host_api::HostCommand,
    ) -> Result<atomcode_host_api::HostReply, atomcode_host_api::HostError> {
        self.asked.lock().expect("poisoned").push(command);
        Ok(self
            .replies
            .lock()
            .expect("poisoned")
            .pop_front()
            .unwrap_or(atomcode_host_api::HostReply::Done))
    }
    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent> {
        mpsc::unbounded_channel().1
    }
}

/// A host that answers nothing, for criteria whose sessions are never asked.
///
/// Test-only: a session built by hand has no runtime behind it, and a criterion
/// that needed one would be testing the runtime instead of the thing it names.
#[cfg(test)]
pub(crate) struct SilentHost;

#[cfg(test)]
#[async_trait::async_trait]
impl HostControl for SilentHost {
    async fn call(
        &self,
        _command: atomcode_host_api::HostCommand,
    ) -> Result<atomcode_host_api::HostReply, atomcode_host_api::HostError> {
        Err(atomcode_host_api::HostError::Unavailable)
    }
    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent> {
        mpsc::unbounded_channel().1
    }
}

/// The host ACP presents to the contract.
///
/// The model-resolving closure was already here — it is what `session/new` was
/// given so `/model` could re-resolve a session's configuration. It used to be
/// called from the option handler, which then handed a finished
/// `CodingAgentConfig` to the runtime. That is the host's job, and
/// `HostCommand::SwitchModel` asks for it by name: the closure did not go away,
/// it moved behind the contract where every front end reaches it the same way.
///
/// The rest is deliberately empty. ACP has no settings file of its own, no
/// provider table to list and nobody signed in — `HostConfig`'s defaults say
/// exactly that, and saying it by default is better than inventing answers.
pub(crate) struct AcpHost {
    resolve_model: Option<std::sync::Arc<crate::acp::SessionModelResolver>>,
    /// What this session runs on now, so `current()` and the working directory
    /// survive a model switch.
    current: CodingAgentConfig,
}

impl crate::host::HostConfig for AcpHost {
    fn for_model(&self, model: &str) -> Result<CodingAgentConfig, String> {
        let resolve = self
            .resolve_model
            .as_ref()
            .ok_or("this host does not resolve models")?;
        let mut next = resolve(model).ok_or_else(|| format!("model `{model}` is not available"))?;
        // The session's working directory stays authoritative across a switch —
        // the same rule the option handler kept when it resolved configs itself.
        next.working_dir = self.current.working_dir.clone();
        Ok(next)
    }

    fn current(&self) -> Result<CodingAgentConfig, String> {
        Ok(self.current.clone())
    }
}

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
///
/// Also returns `None` when the first line is a slash-command invocation
/// (`/word …`): known commands are handled before the turn reaches here, so a
/// slash-shaped prompt arriving is an UNKNOWN command that fell through to the
/// kernel — titling the session with the literal `/nope …` string would be
/// wrong. A leading path (`/usr/bin/x …`) is NOT command-shaped (its first
/// token contains a `/`) and still titles normally.
pub fn derive_title(text: &str) -> Option<String> {
    let first_line = text.lines().next().unwrap_or_default();
    if looks_like_slash_command(first_line) {
        return None;
    }
    let name: String = first_line
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

/// Whether `first_line` is a slash-command invocation (`/word` or `/word args`)
/// rather than a real prompt or a leading filesystem path. Command-shaped means:
/// starts with `/`, and its first whitespace-delimited token is `/` + an
/// identifier (`[A-Za-z][A-Za-z0-9_-]*`) with no further `/` — so `/nope` and
/// `/foo bar` match, but `/usr/bin/x` (a path) does not.
fn looks_like_slash_command(first_line: &str) -> bool {
    let Some(rest) = first_line.trim_start().strip_prefix('/') else {
        return false;
    };
    let mut chars = rest.chars();
    // The char immediately after `/` must start an identifier — a leading space
    // (`/ and then`) or digit (`/123`) is not a command shape.
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    // The rest of the first token must be identifier chars; whitespace ends the
    // token (`/foo bar` → command), any other char (e.g. `/` in `/usr/bin/x`)
    // means it is a path/expression, not a command.
    for c in chars {
        if c.is_whitespace() {
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return false;
        }
    }
    true
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
    config: CodingAgentConfig,
    resolve_model: Option<std::sync::Arc<crate::acp::SessionModelResolver>>,
    cwd: std::path::PathBuf,
    config_options: &[SessionConfigOption],
    additional_directories: Vec<std::path::PathBuf>,
) -> Result<SessionId, agent_client_protocol::Error> {
    // The runtime owns session identity: a session-bearing prepare always
    // reports its native id. Missing it means the prepare ran session-less,
    // which cannot round-trip through the ACP lifecycle — fail closed.
    let native_id = runtime
        .session
        .as_ref()
        .map(|info| info.id.clone())
        .ok_or_else(|| {
            agent_client_protocol::util::internal_error("acp: runtime reported no session id")
        })?;
    // The same `connect()` the full-screen UI goes through. ACP stops reading
    // the product's own event enum here: what it sees from now on is what the
    // contract says, which is what every other front end sees.
    let front_end = FrontEnd::new();
    let host: Arc<dyn crate::host::HostConfig> = Arc::new(AcpHost {
        resolve_model,
        current: config.clone(),
    });
    let connection = crate::host::connect(runtime, front_end.clone(), config, Some(host))
        .map_err(agent_client_protocol::util::internal_error)?;
    let atomcode_host_api::HostConnection {
        commands,
        events,
        control,
        ..
    } = connection;
    // One watcher per session for what the host pushes. Only one kind matters
    // to this channel today, and it matters at the end of a turn.
    let persistence_failure = Arc::new(Mutex::new(None));
    {
        let mut watch = control.subscribe();
        let seen = persistence_failure.clone();
        tokio::spawn(async move {
            while let Some(event) = watch.recv().await {
                if let HostEvent::PersistenceFailed { message, .. } = event {
                    *seen.lock().await = Some(message);
                }
            }
        });
    }

    let id = wire_session_id(&native_id);
    sessions.lock().await.insert(
        id.0.to_string(),
        SessionState {
            commands,
            control,
            events: Arc::new(Mutex::new(events)),
            _front_end: front_end,
            persistence_failure,
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
    let commands = {
        sessions
            .lock()
            .await
            .get(session_id)
            .map(|state| state.commands.clone())
    };
    if let Some(commands) = commands {
        let _ = commands.send(AgentCommand::Cancel);
    }
}

/// Stop a session and **wait for it to be stopped**.
///
/// Sending `Shutdown` is not the same as having shut down. The runtime holds
/// the session's lease until it actually stops, and the next thing a caller
/// does — delete the record, resume the same session — needs the lease
/// released. The stream is the signal: the connection's pump ends when the
/// runtime does, which drops the sender and closes this receiver.
///
/// Bounded, because teardown must not hang on a runtime that is already gone:
/// after the wait the caller proceeds either way.
async fn stop_and_wait(state: SessionState) {
    let _ = state.commands.send(AgentCommand::Cancel);
    let _ = state.commands.send(AgentCommand::Shutdown);
    let events = state.events.clone();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
        let mut rx = events.lock().await;
        while rx.recv().await.is_some() {}
    })
    .await;
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
    let state = {
        let mut map = sessions.lock().await;
        map.remove(session_id.0.as_ref())
    };
    if let Some(state) = state {
        stop_and_wait(state).await;
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
    let state = {
        let mut map = sessions.lock().await;
        map.remove(session_id.0.as_ref())
    };
    if let Some(state) = state {
        stop_and_wait(state).await;
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

    /// Build a stub session state (live channels + a silent host) without
    /// spawning a real kernel agent.
    pub(crate) fn stub_session(native_id: &str, cwd: &str) -> SessionState {
        let (commands, _cmd_rx) = mpsc::unbounded_channel();
        let (_ev_tx, events) = mpsc::unbounded_channel();
        SessionState {
            commands,
            control: Arc::new(super::SilentHost),
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _front_end: FrontEnd::new(),
            persistence_failure: Arc::new(Mutex::new(None)),
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
            needs_newer_version: false,
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

    /// `session/cancel` stops the turn through the handle protocol.
    ///
    /// It used to reach into the product runtime's own control channel. What it
    /// sends now is `AgentCommand::Cancel` — the same command the full-screen
    /// UI sends for the same gesture, which is the point of 6.3: one way to
    /// stop a turn, not one per front end.
    #[tokio::test]
    async fn cancel_sends_the_contracts_cancel() {
        let (commands, mut sent) = mpsc::unbounded_channel();
        let (_ev_tx, events) = mpsc::unbounded_channel();
        let state = SessionState {
            commands,
            control: Arc::new(super::SilentHost),
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _front_end: FrontEnd::new(),
            persistence_failure: Arc::new(Mutex::new(None)),
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

        handle_cancel(&sessions, "acp-1").await;

        assert!(matches!(sent.recv().await, Some(AgentCommand::Cancel)));
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
    fn derive_title_skips_unknown_slash_commands_but_keeps_paths() {
        // An unknown slash command that fell through to the kernel must not
        // become the session title.
        assert_eq!(derive_title("/nope do the thing"), None);
        assert_eq!(derive_title("/foo"), None);
        assert_eq!(
            derive_title("  /bar baz"),
            None,
            "leading space still a command"
        );
        // A leading filesystem path is real content and still titles.
        assert_eq!(
            derive_title("/usr/bin/x is missing").as_deref(),
            Some("/usr/bin/x is missing")
        );
        // A bare slash or non-identifier after the slash is not command-shaped.
        assert_eq!(derive_title("/ and then").as_deref(), Some("/ and then"));
        assert_eq!(
            derive_title("/123 numbers").as_deref(),
            Some("/123 numbers")
        );
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
