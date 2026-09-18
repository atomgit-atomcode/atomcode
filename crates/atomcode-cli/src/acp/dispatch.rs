//! `session/new` / `session/resume` handlers, the v1 prompt turn, and the
//! v1 `session/load` replay projection.
//!
//! Session lifecycle entry points shared by the v1 and v2 handler chains. The
//! live session table and its teardown handlers live in
//! [`crate::acp::sessions`]; the `session/list` directory lives in
//! [`crate::acp::discovery`]; mode/config options live in
//! [`crate::acp::options`]. ACP wire ids are `acp-<native id>` so every wire
//! id round-trips to the single native session catalog shared with the
//! CLI/TUI — no second persistence model.

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, ImageContent as AcpImageContent, MessageId, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, SessionConfigOption, SessionId,
    SessionInfoUpdate, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError, Responder};
use atomcode_capabilities::mcp::McpServerConfig;
use atomcode_capabilities::session::CatalogScan;
use atomcode_capabilities::tools::todo::TodoItem;
use atomcode_coding::{
    CodingProviderFactory, CodingRuntimeHandle, RuntimeStartError,
    SessionMode as CodingSessionMode, TurnCompletion,
};
use atomcode_kernel::event::{AgentEvent, StopReason};
use atomcode_kernel::message::ImageContent;
use atomcode_kernel::tool::ToolCall;

use crate::acp::engine::EngineConfig;
use crate::acp::options::session_mode_state;
use crate::acp::replay::ReplayEntry;
use crate::acp::sessions::{
    native_id_from_wire, next_message_id, register_session, validate_additional_directories,
    Sessions,
};
use crate::acp::turn::TurnWire;
use crate::acp::SessionModelResolver;

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

// ── session/new handler ───────────────────────────────────────────────────────

/// Handle a `session/new` request.
///
/// Spawns a kernel session, inserts it into the shared table, and returns the
/// fresh [`SessionId`] to the client.
///
/// `provider_factory` creates a distinct provider for the session. When absent,
/// the native default factory is used. `config_options` is the initial catalog
/// for the session (empty → the agent does not advertise config options).
pub async fn handle_new_session(
    engine: &EngineConfig,
    provider_factory: Option<Arc<dyn CodingProviderFactory>>,
    sessions: &Sessions,
    req: NewSessionRequest,
    config_options: &[SessionConfigOption],
) -> Result<NewSessionResponse, agent_client_protocol::Error> {
    // Client-injected stdio MCP servers are connected into the session's tool
    // catalog (protocol baseline transport); transports this agent does not
    // advertise (http/sse) and malformed stdio entries are surfaced on stderr.
    let (mcp_configs, ignored) = crate::acp::mcp::acp_mcp_server_configs(&req.mcp_servers);
    crate::acp::mcp::log_ignored_mcp_server_names(&ignored);
    validate_additional_directories(&req.additional_directories)?;
    let id = spawn_and_register_session(
        engine,
        provider_factory,
        sessions,
        req.cwd.clone(),
        config_options,
        mcp_configs,
        req.additional_directories.clone(),
        CodingSessionMode::Fresh,
    )
    .await?;
    let mut resp = NewSessionResponse::new(id).modes(session_mode_state());
    if !config_options.is_empty() {
        resp = resp.config_options(config_options.to_vec());
    }
    Ok(resp)
}

/// Run the `prepare → assemble → spawn` pipeline and register the live session
/// in the shared table. Shared between the v1 and v2 handler chains (the wire
/// request/response shapes differ, the session lifecycle does not).
///
/// The wire id is minted from the runtime's NATIVE session id (`acp-<id>`), so
/// `session/new` and `session/resume` produce ids that round-trip to the native
/// catalog. The failure mapping is shared: spawn errors surface as internal
/// errors unless the caller maps them (see [`handle_resume_session`] for the
/// resume-specific mapping).
#[allow(clippy::too_many_arguments)] // spawn context (engine/provider/cwd/options) is inherent
pub async fn spawn_and_register_session(
    engine: &EngineConfig,
    provider_factory: Option<Arc<dyn CodingProviderFactory>>,
    sessions: &Sessions,
    cwd: std::path::PathBuf,
    config_options: &[SessionConfigOption],
    extra_mcp_servers: Vec<McpServerConfig>,
    additional_directories: Vec<std::path::PathBuf>,
    session: CodingSessionMode,
) -> Result<SessionId, agent_client_protocol::Error> {
    // Protocol MUST (session-setup): `cwd` must be an absolute path. A relative
    // path would silently resolve against the agent process's own directory,
    // breaking the session's filesystem-root contract. Validate BEFORE spawning
    // anything — this is the shared entry point for the v1 and v2 chains.
    if !cwd.is_absolute() {
        return Err(AcpError::invalid_params().data(format!(
            "cwd must be an absolute path (got `{}`)",
            cwd.display()
        )));
    }
    validate_additional_directories(&additional_directories)?;
    let runtime = crate::acp::engine::spawn_session(
        engine,
        cwd.clone(),
        provider_factory,
        extra_mcp_servers,
        session,
    )
    .await
    .map_err(|e| agent_client_protocol::util::internal_error(format!("{e}")))?;
    register_session(
        sessions,
        runtime,
        cwd,
        config_options,
        additional_directories,
    )
    .await
}

// ── session/resume handler ────────────────────────────────────────────────────

/// Handle a `session/resume` request over the native session catalog.
///
/// Failure semantics (fail-closed, no silent fresh):
/// - a wire id this agent did not mint → invalid params;
/// - no persisted session under that id → invalid params;
/// - the request `cwd` differs from the stored working directory → invalid
///   params (the protocol pins `cwd` to the session's);
/// - the session is live in this or another process (lease held) → invalid
///   params with the in-use explanation;
/// - snapshot missing/corrupt/version-mismatched after a successful catalog
///   lookup → internal error (a race, never a silent fresh start).
///
/// Shared by the v1 and v2 chains; the caller builds its own wire response
/// shape (v1 echoes modes/config options, v2 responds `{}`).
///
/// Whether the resume request's `cwd` refers to the same directory as the
/// session's stored `working_dir`. A raw `PathBuf` equality is too strict: a
/// real client (e.g. an editor) may supply an equivalent-but-not-byte-identical
/// path — a trailing separator, a redundant `.` segment, a `..`, or a symlinked
/// project root — and would be wrongly rejected from resuming its own session.
///
/// Matching order (cheap → expensive):
/// 1. exact bytes (also covers the case where neither path exists on disk);
/// 2. component-wise equality — normalizes away trailing separators and `.`
///    segments WITHOUT touching the filesystem and WITHOUT case-folding (a
///    lexical same-path check);
/// 3. [`pathnorm::canonicalize`] on both — resolves symlinks, `..`, and (on a
///    case-insensitive filesystem) case differences to the real on-disk path;
///    requires the paths to exist and falls through to "not equal" otherwise.
///
/// Deliberately does NOT case-fold lexically: the storage bucket key
/// (`stable_project_hash`) case-folds only on Windows, so folding here on other
/// platforms would admit a `cwd` the storage layer treats as a *different*
/// project (the snapshot lives in another bucket) — a case-insensitive FS is
/// instead handled correctly by `canonicalize` resolving to one real path.
fn cwd_matches(stored: &std::path::Path, requested: &std::path::Path) -> bool {
    use atomcode_capabilities::pathnorm;
    if stored == requested || stored.components().eq(requested.components()) {
        return true;
    }
    matches!(
        (pathnorm::canonicalize(stored), pathnorm::canonicalize(requested)),
        (Ok(a), Ok(b)) if a == b
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_resume_session(
    engine: &EngineConfig,
    provider_factory: Option<Arc<dyn CodingProviderFactory>>,
    sessions: &Sessions,
    wire_id: &SessionId,
    cwd: std::path::PathBuf,
    config_options: &[SessionConfigOption],
    extra_mcp_servers: Vec<McpServerConfig>,
    additional_directories: Vec<std::path::PathBuf>,
    scan: &CatalogScan,
) -> Result<SessionId, agent_client_protocol::Error> {
    let native_id = native_id_from_wire(wire_id).ok_or_else(|| {
        AcpError::invalid_params().data(format!("unknown session `{}`", wire_id.0))
    })?;
    if !cwd.is_absolute() {
        return Err(AcpError::invalid_params().data(format!(
            "cwd must be an absolute path (got `{}`)",
            cwd.display()
        )));
    }
    validate_additional_directories(&additional_directories)?;
    let entry = scan
        .find(native_id)
        .map_err(|e| agent_client_protocol::util::internal_error(format!("catalog error: {e}")))?
        .ok_or_else(|| {
            AcpError::invalid_params().data(format!("unknown session `{}`", wire_id.0))
        })?;
    if !cwd_matches(&entry.working_dir, &cwd) {
        return Err(AcpError::invalid_params().data(format!(
            "session `{}` belongs to `{}`; the resume request supplied `{}`",
            wire_id.0,
            entry.working_dir.display(),
            cwd.display()
        )));
    }
    match crate::acp::engine::spawn_session(
        engine,
        cwd.clone(),
        provider_factory,
        extra_mcp_servers,
        CodingSessionMode::Resume(native_id.to_string()),
    )
    .await
    {
        Ok(runtime) => {
            register_session(
                sessions,
                runtime,
                cwd,
                config_options,
                additional_directories,
            )
            .await
        }
        Err(e) => Err(map_resume_start_error(e)),
    }
}

/// Map a resume spawn failure to a JSON-RPC error. `SessionInUse` is the one
/// protocol-relevant case (the session is live elsewhere — a lease conflict
/// must be explicit, never a takeover); everything else is an internal failure
/// (missing/corrupt snapshot after a successful catalog lookup is a race,
/// never a reason to silently fresh-start).
fn map_resume_start_error(e: RuntimeStartError) -> agent_client_protocol::Error {
    match e {
        RuntimeStartError::SessionInUse { .. } => AcpError::invalid_params()
            .data("session is active in another atomcode process; close it before resuming"),
        other => agent_client_protocol::util::internal_error(format!("resume failed: {other}")),
    }
}

// ── session/load replay (v1) ──────────────────────────────────────────────────

/// Map neutral [`ReplayEntry`]s onto v1 `session/update` chunk shapes for
/// `session/load` replay. v1 has no full-content `user_message`/`agent_message`
/// updates — history is streamed as `user_message_chunk` / `agent_message_chunk`
/// / `agent_thought_chunk`, one chunk per content block. All chunks of a message
/// share a `messageId` freshly allocated from the per-connection counter; a
/// `messageId` change marks a new message, so text and image blocks of one user
/// message reuse the same id.
pub fn replay_entries_to_v1_updates(
    entries: &[ReplayEntry],
    msg_ids: &AtomicU64,
) -> Vec<SessionUpdate> {
    let next_id = || MessageId::new(next_message_id(msg_ids));
    let mut updates: Vec<SessionUpdate> = Vec::new();
    for entry in entries {
        match entry {
            ReplayEntry::User { text, images } => {
                let id = next_id();
                updates.push(SessionUpdate::UserMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone())))
                        .message_id(Some(id.clone())),
                ));
                for img in images {
                    updates.push(SessionUpdate::UserMessageChunk(
                        ContentChunk::new(ContentBlock::Image(AcpImageContent::new(
                            img.data.clone(),
                            img.media_type.clone(),
                        )))
                        .message_id(Some(id.clone())),
                    ));
                }
            }
            ReplayEntry::Assistant { text } => updates.push(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone())))
                    .message_id(Some(next_id())),
            )),
            ReplayEntry::Thought { text } => updates.push(SessionUpdate::AgentThoughtChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone())))
                    .message_id(Some(next_id())),
            )),
            ReplayEntry::ToolCall {
                id,
                name,
                arguments,
                result,
            } => {
                // Reconstruct the same two-step tool record a live turn emits:
                // a `tool_call` (in_progress) followed by a `tool_call_update`
                // carrying the recorded result (`completed`/`failed`). No
                // message id is involved — tool records key off `toolCallId`.
                updates.push(SessionUpdate::ToolCall(
                    agent_client_protocol::schema::v1::ToolCall::new(
                        agent_client_protocol::schema::v1::ToolCallId::new(id.clone()),
                        name.clone(),
                    )
                    .kind(crate::acp::translate::tool_kind(name))
                    .status(agent_client_protocol::schema::v1::ToolCallStatus::InProgress)
                    .raw_input(crate::acp::replay::raw_input_from_arguments(arguments)),
                ));
                if let Some((content, is_error)) = result {
                    let status = if *is_error {
                        agent_client_protocol::schema::v1::ToolCallStatus::Failed
                    } else {
                        agent_client_protocol::schema::v1::ToolCallStatus::Completed
                    };
                    let content: agent_client_protocol::schema::v1::ToolCallContent =
                        content.clone().into();
                    updates.push(SessionUpdate::ToolCallUpdate(
                        agent_client_protocol::schema::v1::ToolCallUpdate::new(
                            agent_client_protocol::schema::v1::ToolCallId::new(id.clone()),
                            agent_client_protocol::schema::v1::ToolCallUpdateFields::new()
                                .status(status)
                                .content(vec![content]),
                        ),
                    ));
                }
            }
        }
    }
    updates
}

// ── session/prompt turn loop ───────────────────────────────────────────────────

/// Extract the user message text and any image attachments from a prompt
/// request's content blocks.
///
/// Returns `(text, images, has_attachments)` — `has_attachments` is true when
/// the prompt carried ANY non-text block (image, resource link, audio,
/// resource, future variants). Local slash-command handling must only run for
/// attachment-free prompts so it never silently drops content the model
/// should have seen; prompts with attachments always fall through to the
/// kernel turn.
///
/// Text blocks are concatenated in order; image blocks are collected into the
/// kernel's [`ImageContent`] shape (`media_type` ← ACP `mime_type`). A
/// `ResourceLink` (a protocol-baseline block every agent MUST support) is
/// rendered as an inline `[resource: …]` marker so the reference survives into
/// the kernel prompt — the model can then read the resource with its own tools
/// instead of the link being silently dropped. Audio and embedded `Resource`
/// blocks are gated by prompt capabilities we do not advertise, so a
/// conforming client never sends them here; they stay ignored (but still mark
/// the prompt as attachment-bearing).
pub fn prompt_text(req: &PromptRequest) -> (String, Vec<ImageContent>, bool) {
    let mut text = String::new();
    let mut images = Vec::new();
    let mut has_attachments = false;
    for block in &req.prompt {
        match block {
            ContentBlock::Text(t) => text.push_str(&t.text),
            ContentBlock::Image(i) => {
                images.push(ImageContent {
                    media_type: i.mime_type.clone(),
                    data: i.data.clone(),
                });
                has_attachments = true;
            }
            ContentBlock::ResourceLink(link) => {
                text.push_str(&format!("[resource: {} ({})]", link.name, link.uri));
                has_attachments = true;
            }
            _ => {
                // Audio / embedded Resource / future variants: capability-gated
                // away for conforming clients, but their presence still means
                // the prompt is not a bare text command.
                has_attachments = true;
            }
        }
    }
    (text, images, has_attachments)
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
#[allow(clippy::too_many_arguments)] // turn context (wire/session/request/resolvers) is inherent
pub async fn run_prompt_turn(
    cx: ConnectionTo<Client>,
    sessions: Sessions,
    sid: SessionId,
    text: String,
    images: Vec<ImageContent>,
    has_attachments: bool,
    responder: Responder<PromptResponse>,
    auto_approve: bool,
    model_resolver: Option<&SessionModelResolver>,
    effort_resolver: Option<&SessionModelResolver>,
    msg_ids: Arc<std::sync::atomic::AtomicU64>,
    elicitation_form: &std::sync::atomic::AtomicBool,
) -> Result<(), agent_client_protocol::Error> {
    let cancellation = responder.cancellation();
    let mut wire = V1Wire {
        cx: cx.clone(),
        sessions: sessions.clone(),
        sid: sid.clone(),
        responder: Some(responder),
        model_resolver,
        effort_resolver,
        announced: HashSet::new(),
    };
    crate::acp::turn::run_turn(
        &mut wire,
        cx,
        &sessions,
        sid.0.as_ref(),
        text,
        images,
        has_attachments,
        auto_approve,
        &msg_ids,
        elicitation_form,
        cancellation,
    )
    .await
}

/// The v1 chain's protocol surface for the shared turn driver
/// ([`crate::acp::turn`]).
///
/// Owns the v1 `SessionId`, the deferred [`Responder`] (answered exactly once
/// per turn — on an intercepted slash, an unknown session, a dead kernel, or
/// the turn terminal), the slash-command resolvers, and the set of tool calls
/// already announced as `pending` by the approval round-trip.
struct V1Wire<'a> {
    cx: ConnectionTo<Client>,
    sessions: Sessions,
    sid: SessionId,
    responder: Option<Responder<PromptResponse>>,
    model_resolver: Option<&'a SessionModelResolver>,
    effort_resolver: Option<&'a SessionModelResolver>,
    /// Tool calls already announced as `pending` by the approval round-trip.
    /// Their `ToolStarted` must UPDATE the pending record to `in_progress`
    /// instead of creating a second one (protocol flow: tool_call pending →
    /// request_permission → tool_call_update in_progress → completed/failed).
    announced: HashSet<String>,
}

impl TurnWire for V1Wire<'_> {
    type Update = SessionUpdate;

    fn notify(&self, update: Self::Update) -> Result<(), agent_client_protocol::Error> {
        self.cx
            .send_notification(SessionNotification::new(self.sid.clone(), update))
    }

    fn running_update(&self) -> Option<Self::Update> {
        None
    }

    async fn try_slash(
        &mut self,
        text: &str,
        has_attachments: bool,
        msg_id: &str,
    ) -> Result<bool, agent_client_protocol::Error> {
        // Slash commands run locally against session state and end the turn
        // without a model round-trip. Only attachment-free prompts are
        // eligible: a prompt that carries images/resources must reach the
        // kernel whole (a local handler would silently drop the attachments).
        // Unknown `/…` inputs fall through to the kernel.
        if !has_attachments {
            if let Some((cmd, arg)) = crate::acp::commands::parse_slash_command(text) {
                if let Some(reply) = crate::acp::commands::execute_slash_command(
                    cmd,
                    arg,
                    &self.sessions,
                    &self.cx,
                    &self.sid,
                    self.model_resolver,
                    self.effort_resolver,
                )
                .await
                {
                    self.cx.send_notification(SessionNotification::new(
                        self.sid.clone(),
                        SessionUpdate::AgentMessageChunk(
                            agent_client_protocol::schema::v1::ContentChunk::new(
                                ContentBlock::Text(
                                    agent_client_protocol::schema::v1::TextContent::new(reply),
                                ),
                            )
                            .message_id(MessageId::new(msg_id.to_string())),
                        ),
                    ))?;
                    let responder = self
                        .responder
                        .take()
                        .expect("v1 responder use is exclusive");
                    responder.respond(PromptResponse::new(
                        agent_client_protocol::schema::v1::StopReason::EndTurn,
                    ))?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn translate(&self, ev: &AgentEvent, msg_id: &str) -> Option<Self::Update> {
        crate::acp::translate::event_to_update(ev, Some(msg_id))
    }

    async fn handle_approval_request(
        &mut self,
        runtime: &CodingRuntimeHandle,
        req_id: u64,
        payload: serde_json::Value,
        auto_approve: bool,
    ) -> Result<(), agent_client_protocol::Error> {
        // Announce the tool call as `pending` BEFORE the permission
        // round-trip so the client can attach its permission dialog to a known
        // tool-call record. On denial the kernel emits an error `ToolResult`
        // for the same id, which finalizes the pending record as `failed`; on
        // approval the `ToolStarted` path (`on_tool_started`) updates it to
        // `in_progress`.
        let call_id = payload
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let tool = payload
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        if !call_id.is_empty() {
            self.cx.send_notification(SessionNotification::new(
                self.sid.clone(),
                SessionUpdate::ToolCall(
                    agent_client_protocol::schema::v1::ToolCall::new(
                        agent_client_protocol::schema::v1::ToolCallId::new(call_id.to_string()),
                        tool.to_string(),
                    )
                    .kind(crate::acp::translate::tool_kind(tool))
                    .status(agent_client_protocol::schema::v1::ToolCallStatus::Pending),
                ),
            ))?;
            self.announced.insert(call_id.to_string());
        }
        if auto_approve {
            // `--dangerously-skip-permissions`: auto-allow without round-tripping
            // to the client (which would otherwise make the flag a no-op).
            let _ = runtime
                .respond(req_id, serde_json::json!({"decision": "allow"}))
                .await;
        } else if let Err(e) =
            crate::acp::permission::handle_approval(&self.cx, &self.sid, runtime, req_id, payload)
                .await
        {
            // Defense-in-depth: `handle_approval` already fails closed internally
            // and returns `Ok`, but a `?` here would tear the WHOLE connection
            // down (reserved for genuine transport death, NOT an approval
            // hiccup). Deny this call so the kernel unparks, keep the turn.
            eprintln!("acp: approval handling errored ({e}); denying this call, turn continues");
            let _ = runtime
                .respond(req_id, serde_json::json!({"decision": "deny"}))
                .await;
        }
        Ok(())
    }

    async fn handle_user_input_request(
        &mut self,
        runtime: &CodingRuntimeHandle,
        req_id: u64,
        payload: serde_json::Value,
        form_supported: bool,
    ) {
        // Structured user question (`request_user_input` tool): map to
        // `elicitation/create` (form) when the client advertised form support;
        // otherwise answered with Null (fail-closed). Never propagates an error.
        crate::acp::elicitation::handle_request_user_input(
            &self.cx,
            &self.sid,
            runtime,
            req_id,
            payload,
            form_supported,
        )
        .await;
    }

    fn on_tool_started(&mut self, call: &ToolCall) -> Option<Self::Update> {
        // The approval round-trip already announced this call as `pending`;
        // transition it to `in_progress` instead of creating a duplicate record
        // via the translation path.
        if self.announced.remove(&call.id) {
            Some(SessionUpdate::ToolCallUpdate(
                agent_client_protocol::schema::v1::ToolCallUpdate::new(
                    agent_client_protocol::schema::v1::ToolCallId::new(call.id.clone()),
                    agent_client_protocol::schema::v1::ToolCallUpdateFields::new()
                        .status(agent_client_protocol::schema::v1::ToolCallStatus::InProgress),
                ),
            ))
        } else {
            None
        }
    }

    fn plan_update(&self, todos: &[TodoItem], _native_id: &str) -> Self::Update {
        crate::acp::commands::plan_update_from_todos(todos)
    }

    fn session_info_update(&self, title: &str) -> Self::Update {
        SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title))
    }

    fn unknown_session(&mut self) -> Result<(), agent_client_protocol::Error> {
        self.responder
            .take()
            .expect("v1 responder use is exclusive")
            .respond_with_internal_error("acp: unknown session")
    }

    fn kernel_dead(&mut self, _msg_id: &str) -> Result<(), agent_client_protocol::Error> {
        self.responder
            .take()
            .expect("v1 responder use is exclusive")
            .respond_with_internal_error("acp: kernel agent is no longer running")
    }

    fn finish(
        &mut self,
        terminal: Result<TurnCompletion, StopReason>,
        last_error: Option<String>,
        _msg_id: &str,
    ) -> Result<(), agent_client_protocol::Error> {
        let response = match terminal {
            Ok(completion) => prompt_completion_terminal(&completion, last_error),
            Err(stop) => prompt_terminal(stop, last_error),
        };
        let responder = self
            .responder
            .take()
            .expect("v1 responder use is exclusive");
        match response {
            Ok(sr) => responder.respond(PromptResponse::new(sr)),
            Err(message) => responder.respond_with_internal_error(message),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::sessions::wire_session_id;

    #[test]
    fn cwd_matches_accepts_equivalent_spellings_and_rejects_others() {
        use std::path::Path;
        // Exact match.
        assert!(cwd_matches(
            Path::new("/home/u/proj"),
            Path::new("/home/u/proj")
        ));
        // Trailing separator — same directory (component-wise equality).
        assert!(cwd_matches(
            Path::new("/home/u/proj"),
            Path::new("/home/u/proj/")
        ));
        assert!(cwd_matches(
            Path::new("/home/u/proj/"),
            Path::new("/home/u/proj")
        ));
        // Redundant `.` segment — same directory, lexically.
        assert!(cwd_matches(
            Path::new("/home/u/proj"),
            Path::new("/home/u/./proj")
        ));
        // A genuinely different directory is still rejected.
        assert!(!cwd_matches(
            Path::new("/home/u/proj"),
            Path::new("/home/u/other")
        ));
        // A sibling that merely shares a prefix is rejected (no substring match).
        assert!(!cwd_matches(
            Path::new("/home/u/proj"),
            Path::new("/home/u/proj2")
        ));
        // No lexical case-folding: a case difference on nonexistent paths is NOT
        // a lexical match (a case-insensitive FS is handled by canonicalize only
        // when the dirs actually exist). This keeps the guard from admitting a
        // cwd the case-sensitive storage bucket key would treat as a different
        // project.
        assert!(!cwd_matches(
            Path::new("/home/u/Proj"),
            Path::new("/home/u/proj")
        ));
    }

    #[test]
    fn replay_entries_map_to_v1_chunks_with_shared_message_ids() {
        let entries = vec![
            ReplayEntry::User {
                text: "hi".into(),
                images: vec![ImageContent {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }],
            },
            ReplayEntry::Thought {
                text: "thinking".into(),
            },
            ReplayEntry::Assistant {
                text: "there".into(),
            },
        ];
        let msg_ids = AtomicU64::new(0);
        let updates = replay_entries_to_v1_updates(&entries, &msg_ids);

        let tag = |u: &SessionUpdate| {
            serde_json::to_value(u).unwrap()["sessionUpdate"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let msg_id = |u: &SessionUpdate| {
            serde_json::to_value(u).unwrap()["messageId"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // user text chunk + image chunk, then thought, then assistant text.
        assert_eq!(updates.len(), 4);
        assert_eq!(tag(&updates[0]), "user_message_chunk");
        assert_eq!(tag(&updates[1]), "user_message_chunk");
        assert_eq!(tag(&updates[2]), "agent_thought_chunk");
        assert_eq!(tag(&updates[3]), "agent_message_chunk");
        // A user message's text and image reuse the same messageId; the
        // following messages get distinct ids (messageId change = new message).
        assert_eq!(msg_id(&updates[0]), msg_id(&updates[1]));
        assert_ne!(msg_id(&updates[1]), msg_id(&updates[2]));
        assert_ne!(msg_id(&updates[2]), msg_id(&updates[3]));
    }

    #[test]
    fn replay_tool_call_entries_map_to_v1_tool_records() {
        // A completed call emits `tool_call` (in_progress) followed by a
        // `tool_call_update` (completed) with the recorded result; a call with
        // no persisted result (e.g. cancelled mid-call) stays in_progress.
        let entries = vec![
            ReplayEntry::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"cmd":"ls"}"#.into(),
                result: Some(("file list".into(), false)),
            },
            ReplayEntry::ToolCall {
                id: "call-2".into(),
                name: "grep".into(),
                arguments: "{}".into(),
                result: None,
            },
        ];
        let msg_ids = AtomicU64::new(0);
        let updates = replay_entries_to_v1_updates(&entries, &msg_ids);

        let tag = |u: &SessionUpdate| {
            serde_json::to_value(u).unwrap()["sessionUpdate"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(updates.len(), 3);
        // call-1: start + result.
        assert_eq!(tag(&updates[0]), "tool_call");
        let start = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(start["toolCallId"], "call-1");
        assert_eq!(start["status"], "in_progress");
        assert_eq!(start["kind"], "execute");
        assert_eq!(start["rawInput"]["cmd"], "ls");
        assert_eq!(tag(&updates[1]), "tool_call_update");
        let result = serde_json::to_value(&updates[1]).unwrap();
        assert_eq!(result["toolCallId"], "call-1");
        assert_eq!(result["status"], "completed");
        assert_eq!(result["content"][0]["content"]["text"], "file list");
        // call-2: no result → only the in_progress start remains.
        assert_eq!(tag(&updates[2]), "tool_call");
        let dangling = serde_json::to_value(&updates[2]).unwrap();
        assert_eq!(dangling["toolCallId"], "call-2");
        assert_eq!(dangling["status"], "in_progress");
    }

    #[tokio::test]
    async fn relative_cwd_is_rejected_before_spawning() {
        // ACP requires `cwd` to be an absolute path (session-setup). The shared
        // v1/v2 session entry point must reject a relative path with an
        // Invalid params error BEFORE spawning a kernel agent or registering
        // anything in the session table.
        let engine = EngineConfig::from_coding_config(atomcode_coding::CodingAgentConfig::new(
            "k",
            "https://example.test/v1",
            "m",
            "/original",
        ));
        let sessions: Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let err = spawn_and_register_session(
            &engine,
            None,
            &sessions,
            std::path::PathBuf::from("relative/dir"),
            &[],
            Vec::new(),
            Vec::new(),
            CodingSessionMode::Fresh,
        )
        .await
        .unwrap_err();

        let text = err.to_string();
        assert!(
            text.contains("absolute"),
            "error should explain the absolute-path requirement: {text}"
        );
        // Nothing spawned and no session registered.
        assert!(sessions.lock().await.is_empty());
    }

    #[test]
    fn prompt_text_concatenates_text_blocks_and_collects_images() {
        use agent_client_protocol::schema::v1::{
            ContentBlock, ImageContent, PromptRequest, TextContent,
        };
        let req = PromptRequest::new(
            SessionId::new("acp-test"),
            vec![
                ContentBlock::Text(TextContent::new("hello ")),
                ContentBlock::Text(TextContent::new("world")),
                ContentBlock::Image(ImageContent::new("BASE64", "image/png")),
            ],
        );
        let (text, images, has_attachments) = prompt_text(&req);
        assert_eq!(text, "hello world");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].data, "BASE64");
        assert!(
            has_attachments,
            "image block marks the prompt attachment-bearing"
        );
    }

    #[test]
    fn prompt_text_marks_pure_text_prompts_as_attachment_free() {
        use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, TextContent};
        let req = PromptRequest::new(
            SessionId::new("acp-test"),
            vec![ContentBlock::Text(TextContent::new("/status"))],
        );
        let (text, images, has_attachments) = prompt_text(&req);
        assert_eq!(text, "/status");
        assert!(images.is_empty());
        assert!(
            !has_attachments,
            "pure text prompts stay eligible for local slash handling"
        );
    }

    #[test]
    fn prompt_text_renders_resource_link_inline_marker() {
        use agent_client_protocol::schema::v1::{
            ContentBlock, PromptRequest, ResourceLink, TextContent,
        };
        // ResourceLink is a protocol-baseline block: it must not be silently
        // dropped. It is rendered as an inline marker so the kernel prompt
        // carries the reference (the model can read it with its own tools).
        let req = PromptRequest::new(
            SessionId::new("acp-test"),
            vec![
                ContentBlock::Text(TextContent::new("check ")),
                ContentBlock::ResourceLink(ResourceLink::new(
                    "main.py",
                    "file:///home/user/project/main.py",
                )),
            ],
        );
        let (text, images, has_attachments) = prompt_text(&req);
        assert_eq!(
            text,
            "check [resource: main.py (file:///home/user/project/main.py)]"
        );
        assert!(images.is_empty());
        assert!(
            has_attachments,
            "resource link marks the prompt attachment-bearing"
        );
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
    async fn resume_rejects_unknown_session_non_acp_id_and_cwd_mismatch() {
        use crate::acp::sessions::test_support::{catalog_entry, empty_scan};

        let engine = EngineConfig::from_coding_config(atomcode_coding::CodingAgentConfig::new(
            "k",
            "https://example.test/v1",
            "m",
            "/original",
        ));
        let sessions: Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        // Unknown session (empty catalog) → invalid params.
        let err = handle_resume_session(
            &engine,
            None,
            &sessions,
            &SessionId::new("acp-missing"),
            std::path::PathBuf::from("/work"),
            &[],
            Vec::new(),
            Vec::new(),
            &empty_scan(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown session"));

        // A wire id this agent did not mint → invalid params.
        let err = handle_resume_session(
            &engine,
            None,
            &sessions,
            &SessionId::new("raw-uuid"),
            std::path::PathBuf::from("/work"),
            &[],
            Vec::new(),
            Vec::new(),
            &empty_scan(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown session"));

        // cwd mismatch: the stored session lives in /a, the request says /b.
        let scan = CatalogScan {
            entries: vec![catalog_entry("s1", "stored", "/a")],
            diagnostics: Vec::new(),
        };
        let err = handle_resume_session(
            &engine,
            None,
            &sessions,
            &wire_session_id("s1"),
            std::path::PathBuf::from("/b"),
            &[],
            Vec::new(),
            Vec::new(),
            &scan,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("belongs to"));
    }

    #[tokio::test]
    async fn resume_rejects_relative_cwd() {
        use crate::acp::sessions::test_support::catalog_entry;

        let engine = EngineConfig::from_coding_config(atomcode_coding::CodingAgentConfig::new(
            "k",
            "https://example.test/v1",
            "m",
            "/original",
        ));
        let sessions: Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let scan = CatalogScan {
            entries: vec![catalog_entry("s1", "stored", "/a")],
            diagnostics: Vec::new(),
        };
        let err = handle_resume_session(
            &engine,
            None,
            &sessions,
            &wire_session_id("s1"),
            std::path::PathBuf::from("relative/dir"),
            &[],
            Vec::new(),
            Vec::new(),
            &scan,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }
}
