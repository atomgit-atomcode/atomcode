//! Session table and `session/new` handler.
//!
//! Owns the shared [`Sessions`] map and the monotone session counter.  The
//! handler is wired into the ACP builder in [`crate::acp::serve_stdio`]; Tasks 7-9
//! add their own handlers that share the same table and counter.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId,
    SessionNotification,
};
use agent_client_protocol::{Client, ConnectionTo, Responder};
use atomcode_coding::{
    CodingProviderFactory, CodingRuntime, CodingRuntimeEvent, CodingRuntimeEvents,
    CodingRuntimeHandle, TurnCompletion, UserInput,
};
use atomcode_kernel::event::{AgentEvent, StopReason};
use atomcode_kernel::message::ImageContent;
use tokio::sync::Mutex;

use crate::acp::engine::EngineConfig;

fn prompt_terminal(
    stop: StopReason,
    last_error: Option<String>,
) -> Result<agent_client_protocol::schema::v1::StopReason, String> {
    crate::acp::translate::stop_reason(stop)
        .map_err(|fallback| last_error.unwrap_or_else(|| fallback.to_string()))
}

fn prompt_completion_terminal(
    completion: &TurnCompletion,
    last_error: Option<String>,
) -> Result<agent_client_protocol::schema::v1::StopReason, String> {
    match completion {
        TurnCompletion::Completed { reason, .. } => prompt_terminal(*reason, last_error),
        TurnCompletion::SnapshotUnavailable { reason, error, .. } => Err(format!(
            "{} (turn completion: SnapshotUnavailable, reason: {reason:?})",
            error.message
        )),
    }
}

// ── Session table ─────────────────────────────────────────────────────────────

/// Per-session state held in the shared table.
///
/// The kernel [`AgentHandle`] is split into its three fields so the turn loop
/// (Task 7) can own/lock the `events` receiver for the whole turn **without**
/// holding the [`Sessions`] map lock, while `session/cancel` (Task 9) can still
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
}

/// Live ACP sessions, keyed by session id.
///
/// KNOWN LIMITATION (v1): sessions are never individually pruned — ACP has no
/// `session/close`, so each `session/new` adds a kernel agent that lives until
/// the whole connection ends (all are freed when the process exits / the client
/// disconnects). Per-session teardown (e.g. dropping a session when its kernel
/// task completes) is deferred to a later phase.
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
/// `provider_factory` creates a distinct provider for the session. When absent,
/// the native default factory is used.
pub async fn handle_new_session(
    engine: &EngineConfig,
    provider_factory: Option<Arc<dyn CodingProviderFactory>>,
    sessions: &Sessions,
    counter: &std::sync::atomic::AtomicU64,
    req: NewSessionRequest,
) -> Result<NewSessionResponse, agent_client_protocol::Error> {
    let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let id = new_session_id(n);
    let runtime = crate::acp::engine::spawn_session(engine, req.cwd.clone(), provider_factory)
        .await
        .map_err(|e| agent_client_protocol::util::internal_error(format!("{e}")))?;
    let CodingRuntime {
        handle,
        events,
        task,
        ..
    } = runtime;
    sessions.lock().await.insert(
        id.0.to_string(),
        SessionState {
            runtime: handle,
            events: Arc::new(Mutex::new(events)),
            _task: task,
        },
    );
    Ok(NewSessionResponse::new(id))
}

// ── session/prompt turn loop ───────────────────────────────────────────────────

/// Extract the user message text and any image attachments from a prompt
/// request's content blocks.
///
/// Text blocks are concatenated in order; image blocks are collected into the
/// kernel's [`ImageContent`] shape (`media_type` ← ACP `mime_type`). All other
/// block kinds (audio, resource links, embedded resources) are ignored — they
/// have no kernel-side representation on this turn path.
pub fn prompt_text(req: &PromptRequest) -> (String, Vec<ImageContent>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in &req.prompt {
        match block {
            ContentBlock::Text(t) => text.push_str(&t.text),
            ContentBlock::Image(i) => images.push(ImageContent {
                media_type: i.mime_type.clone(),
                data: i.data.clone(),
            }),
            _ => {}
        }
    }
    (text, images)
}

/// Drive one `session/prompt` turn to completion.
///
/// Runs **off** the dispatch event loop (spawned via `cx.spawn` by the handler
/// in [`crate::acp::serve_stdio`]) so a mid-turn `session/cancel` and the client's
/// permission responses can still be processed by the loop. This function owns
/// the deferred [`Responder`] and answers it exactly once on every exit path.
///
/// Turn-level failures (a kernel `Error` event, an abnormal stop reason, or an
/// approval round-trip failure) respond to the prompt with a JSON-RPC error (or
/// fail the one tool call closed) but return `Ok(())` — returning `Err` from a
/// spawned task tears the whole connection down, wiping the client's thread.
/// That is reserved for genuine transport death (`?` on `send_notification`,
/// where the wire is already broken). An approval hiccup — the client cancelled
/// the prompt, ESC'd, or sent an unexpected message — is NOT transport death:
/// `handle_approval` fails closed internally and the call site here also guards,
/// so a denied permission never crashes the session.
pub async fn run_prompt_turn(
    cx: ConnectionTo<Client>,
    sessions: Sessions,
    sid: SessionId,
    text: String,
    images: Vec<ImageContent>,
    responder: Responder<PromptResponse>,
    auto_approve: bool,
) -> Result<(), agent_client_protocol::Error> {
    // Take what the turn needs (clonable command sender + the events mutex Arc),
    // then release the map lock so it is never held across the turn.
    let (runtime, events) = {
        let map = sessions.lock().await;
        match map.get(sid.0.as_ref()) {
            Some(st) => (st.runtime.clone(), Arc::clone(&st.events)),
            None => return responder.respond_with_internal_error("acp: unknown session"),
        }
    };

    // Lock the receiver BEFORE enqueuing this turn's message: one prompt runs per
    // session at a time, so a concurrent same-session prompt blocks here on the
    // events mutex and cannot interleave its `SendMessage` into the kernel ahead of
    // this turn's recv loop. Locking is therefore serialized; enqueue follows.
    let mut rx = events.lock().await;
    if runtime.submit(UserInput { text, images }).await.is_err() {
        // The kernel agent is gone (panicked / cancelled / session torn down): the
        // prompt will NEVER reach it. We must NOT fall through into the recv loop —
        // a dead kernel drops the events channel, so `rx.recv()` yields `None`, the
        // loop breaks with `StopReason::Stopped`, and the client gets a FALSE
        // `end_turn` success while the prompt silently vanished. Answer the deferred
        // responder with an error exactly once (mirrors the "unknown session" path
        // above). Deliberately NOT an `Err` return: that tears down the whole ACP
        // connection, which is reserved for genuine transport failures — here the
        // client transport is healthy and only the kernel side died.
        return responder.respond_with_internal_error("acp: kernel agent is no longer running");
    }

    // The kernel ALWAYS emits a trailing `TurnComplete` after an `Error` (see
    // kernel `finish_turn`). We must DRAIN that `TurnComplete` rather than return on
    // the `Error`, otherwise it stays buffered in the session's events channel and
    // poisons the NEXT prompt (which would read the stale `TurnComplete` first).
    // Capture the error message and decide the response AFTER the loop.
    let mut last_error: Option<String> = None;
    let terminal = loop {
        match rx.recv().await.map(|event| event.event) {
            Some(CodingRuntimeEvent::Request(request)) if request.kind == "approval" => {
                if auto_approve {
                    // `--dangerously-skip-permissions`: auto-allow without round-tripping
                    // to the client (which would otherwise make the flag a no-op).
                    let _ = runtime
                        .respond(request.id, serde_json::json!({"decision": "allow"}))
                        .await;
                } else if let Err(e) = crate::acp::permission::handle_approval(
                    &cx,
                    &sid,
                    &runtime,
                    request.id,
                    request.payload,
                )
                .await
                {
                    // Defense-in-depth: `handle_approval` already fails closed internally
                    // and returns `Ok`, but a `?` here would tear the WHOLE connection down
                    // (see this function's doc) — reserved for genuine transport death, NOT
                    // an approval hiccup. Deny this call so the kernel unparks, keep the turn.
                    eprintln!(
                        "acp: approval handling errored ({e}); denying this call, turn continues"
                    );
                    let _ = runtime
                        .respond(request.id, serde_json::json!({"decision": "deny"}))
                        .await;
                }
            }
            Some(CodingRuntimeEvent::Request(request)) => {
                // Unknown (non-approval) kernel request kind: we cannot satisfy it.
                // Respond with null (fail-closed) so the kernel unparks and the turn
                // cannot hang waiting for a reply we will never produce.
                eprintln!("acp: unhandled kernel request kind; responding null");
                let _ = runtime.respond(request.id, serde_json::Value::Null).await;
            }
            Some(CodingRuntimeEvent::TurnFinished(completion)) => break Ok(completion),
            Some(CodingRuntimeEvent::Agent(AgentEvent::Error { message, .. })) => {
                // Do NOT return — keep looping so the trailing `TurnComplete` is
                // consumed and cannot poison the next prompt on this session.
                last_error = Some(message);
            }
            Some(CodingRuntimeEvent::Agent(other)) => {
                if let Some(update) = crate::acp::translate::event_to_update(&other) {
                    cx.send_notification(SessionNotification::new(sid.clone(), update))?;
                }
            }
            Some(CodingRuntimeEvent::RuntimeStopped(_)) => {
                last_error = Some("acp: coding runtime stopped before turn terminal".into());
                break Err(StopReason::ProviderError);
            }
            Some(_) => {}
            None => {
                last_error =
                    Some("acp: coding runtime event stream closed before turn terminal".into());
                break Err(StopReason::ProviderError);
            }
        }
    };

    // TurnFinished is authoritative. Budget/loop fuses may emit an AgentError
    // diagnostic immediately before their typed terminal; ACP must still return
    // MaxTurnRequests instead of misreporting an internal provider failure.
    let response = match terminal {
        Ok(completion) => prompt_completion_terminal(&completion, last_error),
        Err(stop) => prompt_terminal(stop, last_error),
    };
    match response {
        Ok(sr) => responder.respond(PromptResponse::new(sr)),
        Err(message) => responder.respond_with_internal_error(message),
    }
}

// ── session/cancel handler ────────────────────────────────────────────────────

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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_id_is_stable_and_unique() {
        assert_eq!(new_session_id(1).0.as_ref(), "acp-1");
        assert_ne!(new_session_id(1).0.as_ref(), new_session_id(2).0.as_ref());
    }

    #[test]
    fn prompt_text_concatenates_text_blocks_and_collects_images() {
        use agent_client_protocol::schema::v1::{
            ContentBlock, ImageContent, PromptRequest, TextContent,
        };
        let req = PromptRequest::new(
            new_session_id(1),
            vec![
                ContentBlock::Text(TextContent::new("hello ")),
                ContentBlock::Text(TextContent::new("world")),
                ContentBlock::Image(ImageContent::new("BASE64", "image/png")),
            ],
        );
        let (text, images) = prompt_text(&req);
        assert_eq!(text, "hello world");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].data, "BASE64");
    }

    #[test]
    fn typed_fuse_terminal_overrides_preceding_error_diagnostic() {
        use agent_client_protocol::schema::v1::StopReason as AcpStop;

        assert_eq!(
            prompt_terminal(StopReason::MaxRounds, Some("max rounds diagnostic".into()),).unwrap(),
            AcpStop::MaxTurnRequests,
        );
        assert_eq!(
            prompt_terminal(
                StopReason::ToolLoopDetected,
                Some("tool loop diagnostic".into()),
            )
            .unwrap(),
            AcpStop::MaxTurnRequests,
        );
        assert_eq!(
            prompt_terminal(
                StopReason::ProviderError,
                Some("provider connection failed".into()),
            )
            .unwrap_err(),
            "provider connection failed",
        );
    }

    #[test]
    fn snapshot_unavailable_is_acp_failure_even_when_reason_is_stopped() {
        let completion = TurnCompletion::SnapshotUnavailable {
            turn_id: 1,
            reason: StopReason::Stopped,
            error: atomcode_coding::RuntimeSnapshotError {
                message: "snapshot failed".into(),
            },
            stats: Default::default(),
        };

        let error = prompt_completion_terminal(&completion, None).unwrap_err();
        assert!(error.contains("snapshot failed"));
        assert!(error.contains("Stopped"));
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
}
