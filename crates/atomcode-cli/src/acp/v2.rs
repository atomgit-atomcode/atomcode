//! ACP draft v2 handler chain (feature-gated `unstable_protocol_v2`).
//!
//! Mirrors the stable v1 chain in [`crate::acp::mod`]/[`crate::acp::dispatch`]
//! with the v2 prompt lifecycle: `session/prompt` is acknowledged immediately
//! with `{}`, progress and completion arrive as `state_update` notifications
//! (`running` → `idle` with a stop reason), and every message chunk carries an
//! agent-owned `messageId`.
//!
//! The router in [`crate::acp::serve_over`] selects this chain when the client
//! negotiates protocol version 2; the v1 chain stays the default. v2 is a
//! draft: it is enabled behind the SDK feature flag and may drift with the
//! spec. The v2 chain shares the session table, engine, provider factory,
//! config-option catalog, and message-id counter with the v1 chain, and
//! reuses the v1 `elicitation/create` types directly because the draft v2
//! elicitation wire format is currently identical to v1 (pinned by
//! `v1_elicitation_wire_deserializes_into_v2_shape`). Not emitted: the v2
//! display-only terminal surface (`terminal_update`/`terminal_output_chunk`).

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use agent_client_protocol::schema::v2::{
    AgentCapabilities, AgentMessage, AgentThought, AvailableCommandsUpdate,
    CancelRequestNotification, CancelSessionNotification, CloseSessionRequest,
    CloseSessionResponse, ConfigOptionUpdate, Content, ContentBlock, ContentChunk,
    DeleteSessionRequest, DeleteSessionResponse, IdleStateUpdate, ImageContent as V2ImageContent,
    Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, McpCapabilities, McpHttpCapabilities, McpServer, MediaType, MessageId,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptImageCapabilities,
    PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse, RunningStateUpdate,
    SessionAdditionalDirectoriesCapabilities, SessionCapabilities, SessionConfigBoolean,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelect, SessionConfigSelectGroup, SessionConfigSelectOption,
    SessionConfigSelectOptions, SessionDeleteCapabilities, SessionId, SessionInfo,
    SessionInfoUpdate, SessionListCursor, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StateUpdate, StopReason, TextContent, ToolCallContent,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolKind, UpdateSessionNotification, UsageUpdate,
    UserMessage,
};
use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, Dispatch, Handled, RequestCancellation,
};
use atomcode_capabilities::mcp::config::McpConfigSource;
use atomcode_capabilities::mcp::{McpServerConfig, McpTransportConfig};
use atomcode_capabilities::session::SessionManager;
use atomcode_capabilities::tools::todo::TodoItem;
use atomcode_coding::{CodingRuntimeHandle, TurnCompletion};
use atomcode_kernel::event::{AgentEvent, StopReason as KernelStop};
use atomcode_kernel::message::ImageContent;
use atomcode_kernel::tool::ToolCall;
use std::path::Path;

use crate::acp::replay::{build_replay_entries, ReplayEntry};
use crate::acp::turn::TurnWire;

use crate::acp::discovery::handle_list_sessions;
use crate::acp::dispatch::{handle_resume_session, spawn_and_register_session};
use crate::acp::permission;
use crate::acp::sessions::{
    handle_cancel, handle_close_session, handle_delete_session, next_message_id, Sessions,
};
use crate::acp::{require_engine, SharedState};

/// Implementation info advertised in the v2 `initialize` response.
fn implementation_info() -> Implementation {
    Implementation::new("atomcode", env!("CARGO_PKG_VERSION")).title("AtomCode")
}

/// v2 capabilities: the baseline session surface (new/list/resume/close/prompt/
/// cancel/update) plus prompt image support, `session/delete`,
/// `additionalDirectories` on session lifecycle requests, and MCP `http`
/// transport.
fn agent_capabilities() -> AgentCapabilities {
    AgentCapabilities::new().session(
        SessionCapabilities::new()
            .prompt(PromptCapabilities::new().image(PromptImageCapabilities::new()))
            .delete(SessionDeleteCapabilities::new())
            .additional_directories(SessionAdditionalDirectoriesCapabilities::new())
            .mcp(McpCapabilities::new().http(McpHttpCapabilities::new())),
    )
}

// ── v1 ↔ v2 config-option boundary conversion ────────────────────────────────
//
// The session table (shared by both chains) stores the v1 `SessionConfigOption`
// catalog as the single storage owner. v2 handlers translate it at the wire
// boundary: advertise the v2 shape, then convert a v2 set-request back to a v1
// set-request so the shared `apply_session_config_option` and the live runtime
// reload path stay the only mutator of session state.

/// Map a v1 config-option category to its v2 twin. Both enums carry the same
/// four named categories plus an untagged `Other(String)` catch-all.
fn v1_to_v2_category(
    c: &agent_client_protocol::schema::v1::SessionConfigOptionCategory,
) -> SessionConfigOptionCategory {
    use agent_client_protocol::schema::v1::SessionConfigOptionCategory as V1Cat;
    match c {
        V1Cat::Mode => SessionConfigOptionCategory::Mode,
        V1Cat::Model => SessionConfigOptionCategory::Model,
        V1Cat::ModelConfig => SessionConfigOptionCategory::ModelConfig,
        V1Cat::ThoughtLevel => SessionConfigOptionCategory::ThoughtLevel,
        V1Cat::Other(s) => SessionConfigOptionCategory::Other(s.clone()),
        _ => SessionConfigOptionCategory::Other(String::new()),
    }
}

/// Convert one v1 `SessionConfigOption` into its v2 wire shape.
fn v1_to_v2_config_option(
    v1: &agent_client_protocol::schema::v1::SessionConfigOption,
) -> SessionConfigOption {
    use agent_client_protocol::schema::v1::{
        SessionConfigKind as V1Kind, SessionConfigSelectOptions as V1SelectOptions,
    };
    let kind = match &v1.kind {
        V1Kind::Select(sel) => {
            let options = match &sel.options {
                V1SelectOptions::Ungrouped(items) => SessionConfigSelectOptions::Ungrouped(
                    items
                        .iter()
                        .map(|o| {
                            let mut opt =
                                SessionConfigSelectOption::new(o.value.0.clone(), o.name.clone());
                            if let Some(desc) = &o.description {
                                opt = opt.description(desc.clone());
                            }
                            opt
                        })
                        .collect(),
                ),
                V1SelectOptions::Grouped(groups) => SessionConfigSelectOptions::Grouped(
                    groups
                        .iter()
                        .map(|g| {
                            SessionConfigSelectGroup::new(
                                g.group.0.clone(),
                                g.name.clone(),
                                g.options
                                    .iter()
                                    .map(|o| {
                                        let mut opt = SessionConfigSelectOption::new(
                                            o.value.0.clone(),
                                            o.name.clone(),
                                        );
                                        if let Some(desc) = &o.description {
                                            opt = opt.description(desc.clone());
                                        }
                                        opt
                                    })
                                    .collect(),
                            )
                        })
                        .collect(),
                ),
                // v1 select options are non-exhaustive; unknown future grouping
                // degrades to an empty select rather than failing the boundary
                // conversion (the catalog is agent-authored, so this is a
                // forward-compat guard, not a live path).
                _ => SessionConfigSelectOptions::Ungrouped(Vec::new()),
            };
            SessionConfigKind::Select(SessionConfigSelect::new(
                sel.current_value.0.clone(),
                options,
            ))
        }
        V1Kind::Boolean(b) => {
            SessionConfigKind::Boolean(SessionConfigBoolean::new(b.current_value))
        }
        _ => unreachable!("v1 config option kind is only select/boolean in the shared catalog"),
    };
    let mut out = SessionConfigOption::new(v1.id.0.clone(), v1.name.clone(), kind);
    if let Some(desc) = &v1.description {
        out = out.description(desc.clone());
    }
    if let Some(cat) = &v1.category {
        out = out.category(v1_to_v2_category(cat));
    }
    out
}

/// Convert the shared v1 config-option catalog into the v2 wire shape.
fn v1_to_v2_config_options(
    v1: &[agent_client_protocol::schema::v1::SessionConfigOption],
) -> Vec<SessionConfigOption> {
    v1.iter().map(v1_to_v2_config_option).collect()
}

/// Convert a v2 config-option value into the v1 shape the shared apply path
/// understands. `None` for v2's `Other` catch-all (fail-closed at the caller:
/// an unknown value shape is an invalid-params error).
fn v2_value_to_v1(
    value: &SessionConfigOptionValue,
) -> Option<agent_client_protocol::schema::v1::SessionConfigOptionValue> {
    use agent_client_protocol::schema::v1::SessionConfigOptionValue as V1Value;
    match value {
        SessionConfigOptionValue::Id { value } => Some(V1Value::value_id(value.0.clone())),
        SessionConfigOptionValue::Boolean { value } => Some(V1Value::boolean(*value)),
        // `Other` (and any future non-exhaustive variant) has no v1 equivalent.
        _ => None,
    }
}

/// Stop reason used for failures v2 cannot express otherwise (provider errors,
/// lost runtimes, closed event streams). Custom values start with `_`.
fn other_stop_reason() -> StopReason {
    StopReason::Other("_error".into())
}

fn v2_stop_reason(r: KernelStop) -> StopReason {
    match r {
        KernelStop::Stopped => StopReason::EndTurn,
        KernelStop::MaxRounds
        | KernelStop::MaxContinuations
        | KernelStop::RepeatLoop
        | KernelStop::ToolLoopDetected => StopReason::MaxTurnRequests,
        KernelStop::Cancelled => StopReason::Cancelled,
        KernelStop::PromptRejected | KernelStop::PolicyDenied => StopReason::Refusal,
        _ => other_stop_reason(),
    }
}

fn tool_kind(name: &str) -> ToolKind {
    let n = name.to_ascii_lowercase();
    if n.contains("read") || n.contains("cat") {
        ToolKind::Read
    } else if n.contains("edit")
        || n.contains("write")
        || n.contains("replace")
        || n.contains("apply")
    {
        ToolKind::Edit
    } else if n.contains("delete") || n.contains("rm") {
        ToolKind::Delete
    } else if n.contains("move") || n.contains("mv") || n.contains("rename") {
        ToolKind::Move
    } else if n.contains("grep") || n.contains("search") || n.contains("glob") || n.contains("find")
    {
        ToolKind::Search
    } else if n.contains("fetch") || n.contains("http") || n.contains("web") {
        ToolKind::Fetch
    } else if n.contains("bash") || n.contains("shell") || n.contains("exec") || n.contains("run") {
        ToolKind::Execute
    } else {
        ToolKind::Other
    }
}

/// Wrap a tool result string as v2 tool-call content (plain text content block).
fn tool_result_content(text: String) -> ToolCallContent {
    ToolCallContent::Content(Box::new(Content::new(ContentBlock::Text(
        TextContent::new(text),
    ))))
}

/// Translate one kernel event to an optional v2 session update.
///
/// Chunk updates are tagged with the current agent message id; tool call
/// updates are upserts keyed by the kernel call id.
pub fn event_to_update(ev: &AgentEvent, message_id: &str) -> Option<SessionUpdate> {
    match ev {
        AgentEvent::TextDelta(s) => Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(s.clone())),
            message_id,
        ))),
        AgentEvent::Reasoning(s) => Some(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(s.clone())),
            message_id,
        ))),
        AgentEvent::ToolStarted { call } => Some(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(ToolCallId::new(call.id.clone()))
                .title(call.name.clone())
                .kind(tool_kind(&call.name))
                .status(ToolCallStatus::InProgress)
                .raw_input(crate::acp::replay::raw_input_from_arguments(&call.arguments)),
        )),
        AgentEvent::ToolResult { result } => {
            let status = if result.is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            Some(SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new(ToolCallId::new(result.call_id.clone()))
                    .status(status)
                    .content(vec![tool_result_content(result.content.clone())]),
            ))
        }
        AgentEvent::Usage(meta) => Some(SessionUpdate::UsageUpdate(UsageUpdate::new(
            u64::from(meta.used_tokens),
            u64::from(meta.ctx_window),
        ))),
        _ => None,
    }
}

/// Build the v2 `session/update` replay sequence for a resumed session
/// (`replayFrom: { "type": "start" }`).
///
/// Reads the persisted native aggregate via the shared
/// [`crate::acp::replay::build_replay_entries`] projection (the same display
/// rules both chains share) and maps each neutral entry to the v2
/// `user_message` / `agent_message` / `agent_thought` update with full `content`
/// arrays, in conversation order. Each message receives a fresh `messageId` from
/// the shared per-connection counter, so replayed ids never collide with live
/// streamed chunks on the same session.
pub fn build_replay_updates(
    native_id: &str,
    working_dir: &Path,
    msg_ids: &AtomicU64,
) -> Result<Vec<SessionUpdate>, agent_client_protocol::Error> {
    let entries = build_replay_entries(native_id, working_dir)
        .map_err(agent_client_protocol::util::internal_error)?;
    Ok(replay_entries_to_v2_updates(&entries, msg_ids))
}

/// Map neutral replay entries onto v2 `session/update` shapes, allocating a
/// fresh `messageId` per message from `msg_ids`.
fn replay_entries_to_v2_updates(
    entries: &[ReplayEntry],
    msg_ids: &AtomicU64,
) -> Vec<SessionUpdate> {
    let next_id = || MessageId::new(next_message_id(msg_ids));
    let mut updates: Vec<SessionUpdate> = Vec::new();
    for entry in entries {
        let content = match entry {
            ReplayEntry::User { text, images } => {
                let mut blocks = vec![ContentBlock::Text(TextContent::new(text.clone()))];
                blocks.extend(images.iter().map(|img| {
                    ContentBlock::Image(V2ImageContent::new(
                        img.data.clone(),
                        MediaType::new(img.media_type.clone()),
                    ))
                }));
                updates.push(SessionUpdate::UserMessage(
                    UserMessage::new(next_id()).content(blocks),
                ));
                continue;
            }
            ReplayEntry::Assistant { text } => {
                vec![ContentBlock::Text(TextContent::new(text.clone()))]
            }
            ReplayEntry::Thought { text } => {
                vec![ContentBlock::Text(TextContent::new(text.clone()))]
            }
            ReplayEntry::ToolCall {
                id,
                name,
                arguments,
                result,
            } => {
                // Reconstruct the same two-step tool record a live turn emits:
                // a `tool_call_update` (in_progress) followed by one carrying
                // the recorded result (`completed`/`failed`). Tool records key
                // off `toolCallId`; no message id is involved.
                updates.push(SessionUpdate::ToolCallUpdate(
                    ToolCallUpdate::new(ToolCallId::new(id.clone()))
                        .title(name.clone())
                        .kind(tool_kind(name))
                        .status(ToolCallStatus::InProgress)
                        .raw_input(crate::acp::replay::raw_input_from_arguments(arguments)),
                ));
                if let Some((content, is_error)) = result {
                    let status = if *is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    };
                    updates.push(SessionUpdate::ToolCallUpdate(
                        ToolCallUpdate::new(ToolCallId::new(id.clone()))
                            .status(status)
                            .content(vec![tool_result_content(content.clone())]),
                    ));
                }
                continue;
            }
        };
        let update = match entry {
            ReplayEntry::Assistant { .. } => {
                SessionUpdate::AgentMessage(AgentMessage::new(next_id()).content(content))
            }
            ReplayEntry::Thought { .. } => {
                SessionUpdate::AgentThought(AgentThought::new(next_id()).content(content))
            }
            ReplayEntry::User { .. } => unreachable!("user handled above"),
            ReplayEntry::ToolCall { .. } => unreachable!("tool call handled above"),
        };
        updates.push(update);
    }
    updates
}

/// Drive one v2 `session/prompt` turn to completion.
///
/// The request was already acknowledged with `{}` by the handler; this task
/// owns the outbound-notification channel and emits the v2 lifecycle: a
/// `running` state update, streamed message/tool updates, then an `idle` state
/// update carrying the stop reason. Failures surface as an `idle` transition
/// with `StopReason::Other` preceded by an error message chunk (there is no
/// JSON-RPC error to return at this point).
#[allow(clippy::too_many_arguments)] // turn context (wire/session/request/flags) is inherent
pub async fn run_prompt_turn_v2(
    cx: ConnectionTo<Client>,
    sessions: Sessions,
    sid: SessionId,
    text: String,
    images: Vec<ImageContent>,
    auto_approve: bool,
    msg_ids: Arc<AtomicU64>,
    cancellation: RequestCancellation,
    elicitation_form: &std::sync::atomic::AtomicBool,
) -> Result<(), agent_client_protocol::Error> {
    let mut wire = V2Wire {
        cx: cx.clone(),
        sid: sid.clone(),
    };
    crate::acp::turn::run_turn(
        &mut wire,
        cx,
        &sessions,
        sid.0.as_ref(),
        text,
        images,
        // v2 has no attachment concept on the wire: `has_attachments` only
        // gates v1's local slash interception, which v2 never runs (slash
        // input reaches the kernel as text), so `false` is always correct.
        false,
        auto_approve,
        &msg_ids,
        elicitation_form,
        cancellation,
    )
    .await
}

/// The v2 chain's protocol surface for the shared turn driver
/// ([`crate::acp::turn`]).
///
/// Owns the v2 `SessionId`. v2 has no local slash commands (slash input goes
/// to the kernel as text) and its tool-call updates are upserts, so unlike the
/// v1 wire there is no slash table and no `announced` set — an approval
/// announcement is itself the record, and the kernel's `ToolStarted` flows
/// through the generic translation as an upsert transition.
struct V2Wire {
    cx: ConnectionTo<Client>,
    sid: SessionId,
}

impl TurnWire for V2Wire {
    type Update = SessionUpdate;

    fn notify(&self, update: Self::Update) -> Result<(), agent_client_protocol::Error> {
        self.cx
            .send_notification(UpdateSessionNotification::new(self.sid.clone(), update))
    }

    fn running_update(&self) -> Option<Self::Update> {
        Some(SessionUpdate::StateUpdate(StateUpdate::Running(
            RunningStateUpdate::new(),
        )))
    }

    async fn try_slash(
        &mut self,
        _text: &str,
        _has_attachments: bool,
        _msg_id: &str,
    ) -> Result<bool, agent_client_protocol::Error> {
        // v2 slash input reaches the kernel as text — never intercepted.
        Ok(false)
    }

    fn translate(&self, ev: &AgentEvent, msg_id: &str) -> Option<Self::Update> {
        event_to_update(ev, msg_id)
    }

    async fn handle_approval_request(
        &mut self,
        runtime: &CodingRuntimeHandle,
        req_id: u64,
        payload: serde_json::Value,
        auto_approve: bool,
    ) -> Result<(), agent_client_protocol::Error> {
        // Announce the tool call as `pending` BEFORE the permission
        // round-trip (v2 tool-call updates are upserts, so this creates the
        // record; the kernel's `ToolStarted` then transitions it to
        // in_progress, and a denial finalizes it with the kernel's error
        // `ToolResult`). Best-effort announcement: a dropped update is an
        // approval hiccup, not transport death — swallow it (mirrors the
        // original v2 loop, which did not propagate here).
        let call_id = payload
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let tool = payload
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        if !call_id.is_empty() {
            let _ = self.notify(SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new(ToolCallId::new(call_id.to_string()))
                    .title(tool.to_string())
                    .kind(tool_kind(tool))
                    .status(ToolCallStatus::Pending),
            ));
        }
        if auto_approve {
            let _ = runtime
                .respond(req_id, serde_json::json!({"decision": "allow"}))
                .await;
        } else if let Err(e) =
            permission::handle_approval_v2(&self.cx, &self.sid, runtime, req_id, payload).await
        {
            eprintln!("acp: v2 approval handling errored ({e}); denying this call, turn continues");
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
        // otherwise answered with Null (fail-closed). The v2 chain reuses the
        // v1 elicitation wire shape (same bridging as the approval path).
        crate::acp::elicitation::handle_request_user_input(
            &self.cx,
            &agent_client_protocol::schema::v1::SessionId::new(self.sid.0.clone()),
            runtime,
            req_id,
            payload,
            form_supported,
        )
        .await;
    }

    fn on_tool_started(&mut self, _call: &ToolCall) -> Option<Self::Update> {
        // v2 tool-call updates are upserts: the pending announcement already
        // created the record, and the kernel's `ToolStarted` arrives as a
        // `ToolCallUpdate(in_progress)` through the generic translation.
        None
    }

    fn plan_update(&self, todos: &[TodoItem], native_id: &str) -> Self::Update {
        crate::acp::commands::plan_update_from_todos_v2(todos, &format!("plan-{native_id}"))
    }

    fn session_info_update(&self, title: &str) -> Self::Update {
        SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title))
    }

    fn unknown_session(&mut self) -> Result<(), agent_client_protocol::Error> {
        self.notify(SessionUpdate::StateUpdate(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(other_stop_reason()),
        )))
    }

    fn kernel_dead(&mut self, msg_id: &str) -> Result<(), agent_client_protocol::Error> {
        let _ = self.notify(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("acp: kernel agent is no longer running")),
            MessageId::new(msg_id.to_string()),
        )));
        self.notify(SessionUpdate::StateUpdate(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(other_stop_reason()),
        )))
    }

    fn finish(
        &mut self,
        terminal: Result<TurnCompletion, KernelStop>,
        last_error: Option<String>,
        msg_id: &str,
    ) -> Result<(), agent_client_protocol::Error> {
        let (stop, error_text) = match terminal {
            Ok(TurnCompletion::Completed { reason, .. }) => (v2_stop_reason(reason), last_error),
            Ok(TurnCompletion::SnapshotUnavailable { reason, error, .. }) => {
                (v2_stop_reason(reason), Some(error.message))
            }
            Err(stop) => (v2_stop_reason(stop), last_error),
        };
        if let Some(text) = error_text {
            let _ = self.notify(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(text)),
                MessageId::new(msg_id.to_string()),
            )));
        }
        self.notify(SessionUpdate::StateUpdate(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(stop),
        )))
    }
}

/// Build the v2 agent chain for [`crate::acp::serve_over`]'s protocol router.
pub(crate) fn build_v2_agent(state: SharedState) -> impl ConnectTo<Client> + 'static {
    let SharedState {
        sessions,
        engine,
        provider_factory,
        auto_approve,
        msg_ids,
        client_elicitation_form,
        config_options,
        model_resolver,
        effort_resolver,
    } = state;
    // Each `async move` handler closure captures the shared counter binding by
    // value; hand every handler its own Arc clone so the later handlers still
    // compile (the prompt handler consumes the original binding).
    let prompt_msg_ids = Arc::clone(&msg_ids);
    let resume_msg_ids = Arc::clone(&msg_ids);
    Agent
        .v2()
        .name("atomcode-v2")
        .on_receive_request(
            {
                let client_elicitation_form = Arc::clone(&client_elicitation_form);
                async move |init: InitializeRequest, responder, _cx| {
                    // Record whether the client supports form elicitation; the
                    // v2 turn loop gates `request_user_input` on it (same as v1).
                    client_elicitation_form.store(
                        init.capabilities
                            .elicitation
                            .as_ref()
                            .and_then(|e| e.form.as_ref())
                            .is_some(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    responder.respond(
                        InitializeResponse::new(init.protocol_version, implementation_info())
                            .capabilities(agent_capabilities()),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let engine = Arc::clone(&engine);
                let provider_factory = provider_factory.clone();
                let config_options = Arc::clone(&config_options);
                async move |req: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let engine_ref = require_engine(&engine)?;
                    // Stdio servers are connected into the session's tool
                    // catalog; transports this agent does not advertise are
                    // surfaced on stderr instead of silently dropped.
                    let (mcp_configs, ignored) = v2_mcp_server_configs(&req.mcp_servers);
                    crate::acp::mcp::log_ignored_mcp_server_names(&ignored);
                    let id = spawn_and_register_session(
                        engine_ref,
                        provider_factory.clone(),
                        &sessions,
                        std::path::PathBuf::from(AsRef::<std::path::Path>::as_ref(&req.cwd)),
                        &config_options,
                        mcp_configs,
                        req.additional_directories
                            .iter()
                            .map(|p| p.0.clone())
                            .collect(),
                        atomcode_coding::SessionMode::Fresh,
                    )
                    .await?;
                    // `config_options` is skipped on the wire when empty, so the
                    // no-catalog case keeps the minimal `{ sessionId }` shape.
                    responder.respond(
                        NewSessionResponse::new(id.clone())
                            .config_options(v1_to_v2_config_options(&config_options)),
                    )?;
                    // Advertise the slash-command surface right after setup
                    // (best-effort, mirroring the v1 chain's reasoning).
                    let _ = cx.send_notification(UpdateSessionNotification::new(
                        id,
                        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                            crate::acp::commands::available_acp_commands_v2(),
                        )),
                    ));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let client_elicitation_form = Arc::clone(&client_elicitation_form);
                async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    // v2: ack immediately, run the lifecycle off the dispatch
                    // loop so cancel/permission responses keep flowing. Clone the
                    // hand-offs so the async closure stays re-callable.
                    let (text, images) = prompt_text_v2(&req);
                    let sid = req.session_id.clone();
                    let turn_sessions = Arc::clone(&sessions);
                    let turn_msg_ids = Arc::clone(&prompt_msg_ids);
                    // The SDK flips this request's cancellation marker even
                    // after the immediate `{}` ack; the turn task watches it
                    // so protocol-level `$/cancel_request` cancels the kernel
                    // like `session/cancel` does.
                    let cancellation = responder.cancellation();
                    cx.spawn({
                        let cx = cx.clone();
                        let client_elicitation_form = Arc::clone(&client_elicitation_form);
                        async move {
                            run_prompt_turn_v2(
                                cx,
                                turn_sessions,
                                sid,
                                text,
                                images,
                                auto_approve,
                                turn_msg_ids,
                                cancellation,
                                client_elicitation_form.as_ref(),
                            )
                            .await
                        }
                    })?;
                    responder.respond(PromptResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = Arc::clone(&sessions);
                async move |notif: CancelSessionNotification, _cx| {
                    handle_cancel(&sessions, notif.session_id.0.as_ref()).await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |cancel: CancelRequestNotification, _cx| {
                // Protocol-level `$/cancel_request` (v2 draft mirrors v1). The
                // SDK flips the matching request's cancellation marker before
                // this handler runs, even for the already-acknowledged
                // `session/prompt`. The v2 turn task watches that marker and
                // cancels the kernel turn (see `run_prompt_turn_v2`), so the
                // client still gets an idle `state_update` with the
                // `cancelled` stop reason — the same terminal as
                // `session/cancel`.
                eprintln!(
                    "acp: v2 $/cancel_request for request {} (marker flipped)",
                    cancel.request_id
                );
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: CloseSessionRequest, responder, _cx| {
                    let v1_sid =
                        agent_client_protocol::schema::v1::SessionId::new(req.session_id.0.clone());
                    let _: agent_client_protocol::schema::v1::CloseSessionResponse =
                        handle_close_session(&sessions, &v1_sid).await;
                    responder.respond(CloseSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: DeleteSessionRequest, responder, _cx| {
                    let v1_sid =
                        agent_client_protocol::schema::v1::SessionId::new(req.session_id.0.clone());
                    let scan = SessionManager::scan_all();
                    handle_delete_session(&sessions, &v1_sid, &scan).await?;
                    responder.respond(DeleteSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: ListSessionsRequest, responder, _cx| {
                    let scan = SessionManager::scan_all();
                    let listed = handle_list_sessions(&sessions, &v1_list_req(&req), &scan).await?;
                    let infos = listed
                        .sessions
                        .into_iter()
                        .map(|s| {
                            let mut info = SessionInfo::new(s.session_id.0.as_ref(), s.cwd);
                            if !s.additional_directories.is_empty() {
                                info = info.additional_directories(s.additional_directories);
                            }
                            info
                        })
                        .collect::<Vec<_>>();
                    let resp = ListSessionsResponse::new(infos)
                        .next_cursor(listed.next_cursor.map(SessionListCursor::new));
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let engine = Arc::clone(&engine);
                let provider_factory = provider_factory.clone();
                let msg_ids = Arc::clone(&resume_msg_ids);
                let config_options = Arc::clone(&config_options);
                async move |req: ResumeSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let engine_ref = require_engine(&engine)?;
                    let v1_sid =
                        agent_client_protocol::schema::v1::SessionId::new(req.session_id.0.clone());
                    let cwd = std::path::PathBuf::from(AsRef::<std::path::Path>::as_ref(&req.cwd));
                    let (mcp_configs, ignored) = v2_mcp_server_configs(&req.mcp_servers);
                    crate::acp::mcp::log_ignored_mcp_server_names(&ignored);
                    let scan = atomcode_capabilities::session::SessionManager::scan_all();
                    // Validate the replay cursor BEFORE any restore side effect:
                    // an unknown/unsupported cursor is rejected outright (per the
                    // schema: "reject rather than guessing where to replay from"),
                    // so a rejected request never leaves a half-restored session.
                    // Only `start` (full conversation) and a missing cursor
                    // (silent restore) are honoured.
                    if let Some(cursor) = req.replay_from.as_ref() {
                        if !matches!(
                            cursor,
                            agent_client_protocol::schema::v2::ReplayFrom::Start(_)
                        ) {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params().data(
                                    "unsupported replayFrom cursor; only `start` is supported",
                                ),
                            );
                        }
                    }
                    // Restore FIRST: replay only makes sense for a session that
                    // actually resumed, and a failed restore must fail closed
                    // before any history is emitted to the client.
                    handle_resume_session(
                        engine_ref,
                        provider_factory.clone(),
                        &sessions,
                        &v1_sid,
                        cwd.clone(),
                        &config_options,
                        mcp_configs,
                        req.additional_directories
                            .iter()
                            .map(|p| p.0.clone())
                            .collect(),
                        &scan,
                    )
                    .await?;
                    // `replayFrom: start`: emit the persisted conversation as
                    // full-content message updates BEFORE responding — the
                    // protocol requires all requested replay entries before the
                    // resume response.
                    if let Some(agent_client_protocol::schema::v2::ReplayFrom::Start(_)) =
                        req.replay_from.as_ref()
                    {
                        let native_id = crate::acp::sessions::native_id_from_wire(&v1_sid)
                            .ok_or_else(|| {
                                agent_client_protocol::util::internal_error(
                                    "acp: resume replay: invalid session id",
                                )
                            })?;
                        let updates = build_replay_updates(native_id, &cwd, &msg_ids)?;
                        for update in updates {
                            cx.send_notification(UpdateSessionNotification::new(
                                req.session_id.clone(),
                                update,
                            ))?;
                        }
                    }
                    // Advertise the initial config catalog (skipped on the wire
                    // when empty), mirroring `session/new`.
                    responder.respond(
                        ResumeSessionResponse::new()
                            .config_options(v1_to_v2_config_options(&config_options)),
                    )?;
                    // Advertise the slash-command surface right after resume
                    // (best-effort, mirroring the v1 chain's reasoning).
                    let _ = cx.send_notification(UpdateSessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                            crate::acp::commands::available_acp_commands_v2(),
                        )),
                    ));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let model_resolver = model_resolver.clone();
                let effort_resolver = effort_resolver.clone();
                async move |req: SetSessionConfigOptionRequest,
                            responder,
                            cx: ConnectionTo<Client>| {
                    let Some(v1_value) = v2_value_to_v1(&req.value) else {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data("unsupported session config option value shape"),
                        );
                    };
                    // Reuse the shared v1 apply path so v1 and v2 mutate the
                    // exact same session state (catalog + runtime reload).
                    let v1_req =
                        agent_client_protocol::schema::v1::SetSessionConfigOptionRequest::new(
                            agent_client_protocol::schema::v1::SessionId::new(
                                req.session_id.0.clone(),
                            ),
                            req.config_id.0.clone(),
                            v1_value,
                        );
                    let resolver = model_resolver.as_deref();
                    let effort = effort_resolver.as_deref();
                    let (catalog, _switched_mode) =
                        crate::acp::options::apply_session_config_option(
                            &sessions, &v1_req, resolver, effort,
                        )
                        .await?;
                    // v2 has no separate current_mode_update; the mode switch
                    // (if any) is reflected by the `config_option_update` carrying
                    // the full updated catalog.
                    let v2_catalog = v1_to_v2_config_options(&catalog);
                    cx.send_notification(UpdateSessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                            v2_catalog.clone(),
                        )),
                    ))?;
                    responder.respond(SetSessionConfigOptionResponse::new(v2_catalog))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, _cx: ConnectionTo<Client>| match message {
                Dispatch::Request(_, responder) => {
                    responder.respond_with_error(agent_client_protocol::util::internal_error(
                        "unhandled request",
                    ))?;
                    Ok(Handled::Yes)
                }
                _ => Ok(Handled::No {
                    message,
                    retry: false,
                }),
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
}

/// Extract the user text + image blocks from a v2 prompt request.
///
/// Mirrors the v1 [`crate::acp::dispatch::prompt_text`]: `ResourceLink`
/// (baseline) is rendered as an inline `[resource: …]` marker so the reference
/// survives into the kernel prompt instead of being silently dropped.
fn prompt_text_v2(req: &PromptRequest) -> (String, Vec<ImageContent>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in &req.prompt {
        match block {
            ContentBlock::Text(t) => text.push_str(&t.text),
            ContentBlock::Image(image) => images.push(ImageContent {
                media_type: image.mime_type.to_string(),
                data: image.data.clone(),
            }),
            ContentBlock::ResourceLink(link) => {
                text.push_str(&format!("[resource: {} ({})]", link.name, link.uri));
            }
            _ => {}
        }
    }
    (text, images)
}

/// Convert client-injected v2 `mcpServers` into coding MCP configs.
///
/// Mirrors the v1 helper in [`crate::acp::mcp::acp_mcp_server_configs`]:
/// stdio servers (the protocol-baseline transport) and HTTP servers (advertised
/// via `mcp_capabilities.http` in [`agent_capabilities`]) are connected;
/// transports this agent does not advertise (MCP-over-ACP `acp`, and unknown
/// `other` transports) are returned in the ignored list for logging.
fn v2_mcp_server_configs(mcp_servers: &[McpServer]) -> (Vec<McpServerConfig>, Vec<String>) {
    let mut configs = Vec::new();
    let mut ignored = Vec::new();
    for server in mcp_servers {
        match server {
            McpServer::Stdio(s) => {
                let command = AsRef::<std::path::Path>::as_ref(&s.command).to_path_buf();
                let relative = !command.is_absolute();
                if s.name.is_empty() || command.as_os_str().is_empty() || relative {
                    ignored.push(s.name.clone());
                    if relative && !s.name.is_empty() {
                        eprintln!(
                            "acp: mcpServer `{}` command `{}` is not an absolute path; not connected",
                            s.name,
                            command.display()
                        );
                    }
                    continue;
                }
                configs.push(McpServerConfig {
                    name: s.name.clone(),
                    disabled: false,
                    config: McpTransportConfig::Stdio {
                        command: command.to_string_lossy().into_owned(),
                        args: s.args.clone(),
                        env: s
                            .env
                            .iter()
                            .map(|e| (e.name.clone(), e.value.clone()))
                            .collect(),
                        timeout_ms: None,
                    },
                    source: McpConfigSource::Driver,
                    trust: false,
                    auto_approve: Vec::new(),
                });
            }
            McpServer::Http(h) => {
                // Advertised via `mcp_capabilities.http` (see `agent_capabilities`).
                // Map to the coding MCP HTTP transport; the client supplies the
                // URL + headers and is the trust boundary (source=Driver). The
                // ACP `McpServer::Http` shape carries no auth metadata, so `auth`
                // stays `None` (unauthenticated HTTP endpoint). Kept in lockstep
                // with the v1 twin in `crate::acp::mcp::acp_mcp_server_configs`.
                configs.push(McpServerConfig {
                    name: h.name.clone(),
                    disabled: false,
                    config: McpTransportConfig::Http {
                        url: h.url.clone(),
                        headers: h
                            .headers
                            .iter()
                            .map(|e| (e.name.clone(), e.value.clone()))
                            .collect(),
                        auth: None,
                        timeout_ms: None,
                    },
                    source: McpConfigSource::Driver,
                    trust: false,
                    auto_approve: Vec::new(),
                });
            }
            // `Acp` (MCP-over-ACP) is feature-gated upstream and this agent
            // does not advertise it; unknown `other` transports keep their raw
            // type discriminator so the client can see what was not connected.
            McpServer::Other(o) => ignored.push(o.type_.clone()),
            _ => {}
        }
    }
    (configs, ignored)
}

/// Convert a v2 list request into its v1 twin (both are plain cwd+cursor
/// filters; the v1 session table paginates).
fn v1_list_req(
    req: &ListSessionsRequest,
) -> agent_client_protocol::schema::v1::ListSessionsRequest {
    let mut v1 = agent_client_protocol::schema::v1::ListSessionsRequest::new();
    if let Some(cwd) = &req.cwd {
        v1 = v1.cwd(std::path::PathBuf::from(AsRef::<std::path::Path>::as_ref(
            cwd,
        )));
    }
    if let Some(cursor) = &req.cursor {
        v1 = v1.cursor(cursor.as_ref().to_string());
    }
    v1
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v2::SessionUpdate as V2Update;
    use atomcode_capabilities::session::presentation::PRESENTATION_VERSION;
    use atomcode_capabilities::session::{
        DisplayAnchor, PresentationEntry, PresentationFile, PresentationRole, SessionMeta,
        StorageOwner, TurnStat,
    };
    use atomcode_kernel::message::{Message, SessionSnapshot};

    fn tag(u: &V2Update) -> &'static str {
        match u {
            V2Update::UserMessage(_) => "user_message",
            V2Update::AgentMessage(_) => "agent_message",
            V2Update::AgentThought(_) => "agent_thought",
            V2Update::UserMessageChunk(_) => "user_message_chunk",
            V2Update::AgentMessageChunk(_) => "agent_message_chunk",
            V2Update::AgentThoughtChunk(_) => "agent_thought_chunk",
            V2Update::ToolCallUpdate(_) => "tool_call_update",
            _ => "other",
        }
    }

    fn text_content(u: &V2Update) -> String {
        let v = serde_json::to_value(u).unwrap();
        v["content"][0]["text"].as_str().unwrap_or("").to_string()
    }

    /// v2 advertises `mcp_capabilities.http`, so client-injected HTTP MCP
    /// servers must be CONNECTED (not silently dropped), staying in lockstep
    /// with the v1 twin `acp_mcp_server_configs`.
    #[test]
    fn v2_mcp_server_configs_connects_stdio_and_http() {
        use agent_client_protocol::schema::v2::{HttpHeader, McpServerHttp, McpServerStdio};
        // An unadvertised `Other` transport lands in `ignored`; the untagged
        // `OtherMcpServer` is non-exhaustive so we materialize it via JSON.
        let other: McpServer =
            serde_json::from_value(serde_json::json!({"type": "sse", "name": "events"})).unwrap();
        let servers = vec![
            McpServer::Stdio(McpServerStdio::new("fs", "/usr/bin/fs-server")),
            McpServer::Http(
                McpServerHttp::new("api", "https://api.example.com/mcp")
                    .headers(vec![HttpHeader::new("Authorization", "Bearer t")]),
            ),
            other,
        ];
        let (configs, ignored) = v2_mcp_server_configs(&servers);
        assert_eq!(
            configs.len(),
            2,
            "stdio (baseline) + advertised http are connected"
        );
        assert_eq!(
            ignored,
            vec!["sse".to_string()],
            "only the unadvertised `other` transport is ignored"
        );
        let http = configs
            .iter()
            .find(|c| c.name == "api")
            .expect("http server connected");
        match &http.config {
            McpTransportConfig::Http { url, headers, auth, .. } => {
                assert_eq!(url, "https://api.example.com/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer t")
                );
                assert!(auth.is_none(), "ACP McpServer::Http carries no auth metadata");
            }
            other => panic!("expected Http transport, got {other:?}"),
        }
        assert_eq!(http.source, McpConfigSource::Driver);
        assert!(!http.trust, "client-injected server routes through kernel approval");
    }

    /// Persist a native session (meta + snapshot + presentation) under a
    /// dedicated ATOMDCODE_HOME so `SessionManager::for_project` resolves the
    /// same bucket the ACP replay reads.
    fn persist_replay_session(
        home: &tempfile::TempDir,
        cwd: &std::path::Path,
        id: &str,
        messages: Vec<Message>,
        presentation: Vec<PresentationEntry>,
        turn_stats: Vec<TurnStat>,
    ) {
        std::env::set_var("ATOMCODE_HOME", home.path());
        let mgr = SessionManager::for_project(cwd);
        let mut meta = SessionMeta::new(id, cwd.to_string_lossy(), 1);
        meta.owner = StorageOwner::Native;
        meta.turn_stats = turn_stats;
        mgr.write_meta(&meta).unwrap();
        mgr.save_snapshot(id, &SessionSnapshot::new(messages))
            .unwrap();
        mgr.write_presentation(
            id,
            &PresentationFile {
                v: PRESENTATION_VERSION,
                entries: presentation,
            },
        )
        .unwrap();
    }

    fn turn_stat(turn_id: u64, after_message: usize) -> TurnStat {
        TurnStat {
            after_message,
            position_valid: true,
            turn_id,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
            model_usage: Vec::new(),
        }
    }

    #[test]
    fn v2_stop_reason_mapping() {
        assert_eq!(v2_stop_reason(KernelStop::Stopped), StopReason::EndTurn);
        assert_eq!(
            v2_stop_reason(KernelStop::MaxRounds),
            StopReason::MaxTurnRequests
        );
        assert_eq!(v2_stop_reason(KernelStop::Cancelled), StopReason::Cancelled);
        assert_eq!(
            v2_stop_reason(KernelStop::PromptRejected),
            StopReason::Refusal
        );
        assert_eq!(
            v2_stop_reason(KernelStop::ProviderError),
            other_stop_reason()
        );
    }

    #[test]
    fn v2_event_to_update_carries_message_id() {
        let u = event_to_update(&AgentEvent::TextDelta("hi".into()), "m1").unwrap();
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["sessionUpdate"], "agent_message_chunk");
        assert_eq!(v["messageId"], "m1");
        assert_eq!(v["content"]["text"], "hi");
    }

    #[test]
    fn v2_usage_update_flat_shape() {
        use atomcode_kernel::message::MessageMeta;
        let meta = MessageMeta {
            tokens: atomcode_kernel::stream::TokenUsage {
                prompt: 1,
                completion: 1,
                cached: 0,
            },
            elapsed_ms: 1,
            reasoning_elapsed_ms: 0,
            ctx_window: 200_000,
            used_tokens: 100,
            utilization: 0.0,
            round: 1,
            turn_id: 1,
            request_id: 1,
            provider_response_id: None,
            provider_model: None,
            session_id: None,
            finish_reason: "stop".into(),
        };
        let u = event_to_update(&AgentEvent::Usage(meta), "m1").unwrap();
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["sessionUpdate"], "usage_update");
        assert_eq!(v["used"], 100);
        assert_eq!(v["size"], 200_000);
    }

    #[test]
    #[serial_test::serial]
    fn replay_builds_ordered_updates_from_snapshot_and_presentation() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut tool_result = Message::user("tool output");
        tool_result.tool_call_id = Some("call-1".into());
        let mut assistant = Message::assistant("hi there", Vec::new());
        assistant.reasoning = Some("thinking...".into());
        persist_replay_session(
            &home,
            cwd.path(),
            "s1",
            vec![
                Message::user("hello"),
                assistant,
                tool_result, // tool result echo → hidden
            ],
            vec![
                PresentationEntry {
                    anchor: DisplayAnchor::AtStart,
                    role: PresentationRole::Assistant,
                    text: "welcome".into(),
                },
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 7 },
                    role: PresentationRole::User,
                    text: "user note".into(),
                },
            ],
            vec![turn_stat(7, 1)], // turn 7 ends after snapshot message 1
        );

        let msg_ids = AtomicU64::new(0);
        let updates = build_replay_updates("s1", cwd.path(), &msg_ids).unwrap();

        let tags: Vec<&str> = updates.iter().map(tag).collect();
        // position 0 (AtStart) → user hello → turn-7 note (position 1) →
        // assistant reasoning + text; tool result hidden.
        assert_eq!(
            tags,
            [
                "agent_message",
                "user_message",
                "user_message",
                "agent_thought",
                "agent_message",
            ]
        );
        assert_eq!(text_content(&updates[0]), "welcome");
        assert_eq!(text_content(&updates[1]), "hello");
        assert_eq!(text_content(&updates[2]), "user note");
        assert_eq!(text_content(&updates[3]), "thinking...");
        assert_eq!(text_content(&updates[4]), "hi there");

        // Every replayed message carries its own unique messageId.
        let ids: Vec<String> = updates
            .iter()
            .map(|u| {
                serde_json::to_value(u).unwrap()["messageId"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(ids.len(), 5);
        let unique: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        assert_eq!(unique.len(), 5, "messageIds must be unique: {ids:?}");
    }

    #[test]
    #[serial_test::serial]
    fn replay_skips_hidden_entries_and_missing_anchors() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        persist_replay_session(
            &home,
            cwd.path(),
            "s2",
            vec![
                Message::user("hello"),
                // Synthetic system reminder — never shown as a user message.
                atomcode_kernel::message::Message::synthetic_user(
                    atomcode_capabilities::reminder::system_reminder("context"),
                ),
                Message::assistant("done", Vec::new()),
            ],
            vec![
                // Anchored at a turn id that has no turn stat → dropped.
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 99 },
                    role: PresentationRole::User,
                    text: "orphan note".into(),
                },
                // System-reminder presentation rows are hidden too.
                PresentationEntry {
                    anchor: DisplayAnchor::AtStart,
                    role: PresentationRole::User,
                    text: atomcode_capabilities::reminder::system_reminder("reminder"),
                },
            ],
            Vec::new(), // no turn stats at all
        );

        let msg_ids = AtomicU64::new(0);
        let updates = build_replay_updates("s2", cwd.path(), &msg_ids).unwrap();

        let tags: Vec<&str> = updates.iter().map(tag).collect();
        assert_eq!(tags, ["user_message", "agent_message"]);
        assert_eq!(text_content(&updates[0]), "hello");
        assert_eq!(text_content(&updates[1]), "done");
    }

    #[test]
    #[serial_test::serial]
    fn replay_reconstructs_tool_calls_with_results() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // An assistant message that made one tool call, plus its recorded
        // result echo (User message carrying the call id).
        let call = atomcode_kernel::tool::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: r#"{"cmd":"ls"}"#.into(),
        };
        let mut assistant = Message::assistant("running", vec![call]);
        assistant.reasoning = Some("deciding...".into());
        let mut tool_result = Message::user("file list");
        tool_result.tool_call_id = Some("call-1".into());
        persist_replay_session(
            &home,
            cwd.path(),
            "s3",
            vec![Message::user("hello"), assistant, tool_result],
            Vec::new(),
            Vec::new(),
        );

        let msg_ids = AtomicU64::new(0);
        let updates = build_replay_updates("s3", cwd.path(), &msg_ids).unwrap();

        // user hello → assistant reasoning + text → tool_call_update
        // (in_progress) → tool_call_update (completed with result content).
        let tags: Vec<&str> = updates.iter().map(tag).collect();
        assert_eq!(
            tags,
            [
                "user_message",
                "agent_thought",
                "agent_message",
                "tool_call_update",
                "tool_call_update"
            ]
        );
        let first = serde_json::to_value(&updates[3]).unwrap();
        assert_eq!(first["sessionUpdate"], "tool_call_update");
        assert_eq!(first["toolCallId"], "call-1");
        assert_eq!(first["title"], "bash");
        assert_eq!(first["status"], "in_progress");
        assert_eq!(first["rawInput"]["cmd"], "ls");
        let second = serde_json::to_value(&updates[4]).unwrap();
        assert_eq!(second["toolCallId"], "call-1");
        assert_eq!(second["status"], "completed");
        assert_eq!(second["content"][0]["content"]["text"], "file list");
    }

    #[test]
    #[serial_test::serial]
    fn replay_pairs_duplicate_tool_call_ids_by_order_not_last_wins() {
        use crate::acp::replay::{build_replay_entries, ReplayEntry};
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // Two tool calls that reuse the SAME id (weak/gateway models do this;
        // an empty id would collide identically). Each has its own result echo.
        let call_a = atomcode_kernel::tool::ToolCall {
            id: "dup".into(),
            name: "bash".into(),
            arguments: r#"{"cmd":"a"}"#.into(),
        };
        let call_b = atomcode_kernel::tool::ToolCall {
            id: "dup".into(),
            name: "bash".into(),
            arguments: r#"{"cmd":"b"}"#.into(),
        };
        let assistant = Message::assistant("running", vec![call_a, call_b]);
        let mut result_a = Message::user("output-A");
        result_a.tool_call_id = Some("dup".into());
        let mut result_b = Message::user("output-B");
        result_b.tool_call_id = Some("dup".into());
        persist_replay_session(
            &home,
            cwd.path(),
            "sdup",
            vec![assistant, result_a, result_b],
            Vec::new(),
            Vec::new(),
        );

        let entries = build_replay_entries("sdup", cwd.path()).unwrap();
        let tool_results: Vec<Option<String>> = entries
            .iter()
            .filter_map(|e| match e {
                ReplayEntry::ToolCall { result, .. } => {
                    Some(result.as_ref().map(|(t, _)| t.clone()))
                }
                _ => None,
            })
            .collect();
        // The first call must resolve to the first result and the second to the
        // second — NOT both to the last ("output-B"), which is what a single
        // last-wins map would produce.
        assert_eq!(
            tool_results,
            vec![Some("output-A".to_string()), Some("output-B".to_string())]
        );
    }

    #[test]
    #[serial_test::serial]
    fn replay_missing_session_is_an_error() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let msg_ids = AtomicU64::new(0);
        let err = build_replay_updates("no-such-session", cwd.path(), &msg_ids).unwrap_err();
        assert!(
            err.to_string().contains("replay failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn v2_capabilities_only_advertise_implemented_surfaces() {
        // The v2 `initialize` response must advertise only what this agent
        // implements: the baseline session surface, prompt image support,
        // `session/delete`, `additionalDirectories`, and MCP `http` transport. It
        // must NOT advertise auth (empty authMethods), MCP `sse` (no SSE transport
        // in the capabilities MCP layer), or any client-side fs/terminal surface
        // (dropped from v2, not implemented).
        let caps = agent_capabilities();
        let json = serde_json::to_value(&caps).unwrap();
        let session = json.get("session").expect("session capability present");
        assert!(
            session.get("prompt").is_some(),
            "prompt capability advertised"
        );
        assert!(
            session.get("delete").is_some(),
            "delete capability advertised"
        );
        assert!(
            session.get("additionalDirectories").is_some(),
            "additionalDirectories capability advertised"
        );
        assert!(
            json.get("auth").map_or(true, serde_json::Value::is_null),
            "auth extension must not be advertised"
        );
        // MCP `http` transport is advertised; `sse` is not (no SSE transport).
        let mcp = session.get("mcp").expect("mcp capability present");
        assert!(
            mcp.get("http").map_or(false, |v| !v.is_null()),
            "http MCP transport advertised: {mcp}"
        );
        assert!(
            mcp.get("sse").map_or(true, serde_json::Value::is_null),
            "sse MCP transport not advertised"
        );
        assert!(
            json.get("fs").is_none(),
            "client-side fs must not be advertised"
        );
        assert!(
            json.get("terminal").is_none(),
            "client-side terminal must not be advertised"
        );
    }

    #[test]
    fn v1_elicitation_wire_deserializes_into_v2_shape() {
        // The v2 chain reuses the v1 `elicitation/create` types (same bridging
        // as the v1 chain). This test pins the reason that is SAFE rather than
        // an approximation: a v1-shaped form request deserializes unchanged into
        // the v2 types, because the draft v2 schema's elicitation wire format is
        // currently identical to v1 (schema crate v2 module doc: "The wire
        // format is currently identical to v1"). If a future schema change makes
        // this fail, that is the signal to split a real v2 elicitation round-trip.
        use agent_client_protocol::schema::v1::{
            CreateElicitationRequest as V1Req, ElicitationFormMode as V1Form,
            ElicitationMode as V1Mode, ElicitationPropertySchema as V1Prop,
            ElicitationSchema as V1Schema, ElicitationScope as V1Scope,
            ElicitationSessionScope as V1SessionScope, StringPropertySchema as V1String,
        };
        use agent_client_protocol::schema::v2::{
            CreateElicitationRequest as V2Req, ElicitationScope as V2Scope,
        };
        let mut schema = V1Schema::new();
        schema = schema.property(
            "answer",
            V1Prop::String(V1String::new().title("Answer")),
            true,
        );
        let mode = V1Mode::Form(V1Form::new(
            V1Scope::Session(V1SessionScope::new("acp-1")),
            schema,
        ));
        let v1 = V1Req::new(mode, "Please answer");
        let json = serde_json::to_value(&v1).unwrap();
        let v2: V2Req = serde_json::from_value(json).unwrap();
        match v2.scope() {
            V2Scope::Session(s) => assert_eq!(s.session_id.0.as_ref(), "acp-1"),
            other => panic!("unexpected v2 scope {other:?}"),
        }
    }
}
