//! ACP (Agent Client Protocol) agent server.
//!
//! Implements the `atomcode acp` stdio server across two protocol generations:
//! a stable v1 handler chain ([`build_v1_agent`]) and a draft v2 chain
//! ([`crate::acp::v2::build_v2_agent`]), selected per connection by the SDK
//! protocol router in [`serve_over`]. The shared session table and prompt turn
//! loop live in [`crate::acp::dispatch`]. Handler/transport notes recorded
//! while wiring the SDK are kept in `docs/acp-sdk-handler-notes.md`.

pub mod commands;
pub mod discovery;
pub mod dispatch;
pub mod elicitation;
pub mod engine;
pub mod mcp;
pub mod options;
pub mod permission;
pub mod replay;
pub mod sessions;
pub mod translate;
pub mod turn;
pub mod v2;

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommandsUpdate, CancelNotification, CancelRequestNotification,
    CloseSessionRequest, DeleteSessionRequest, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, NewSessionRequest, PromptCapabilities, PromptRequest, ResumeSessionRequest,
    ResumeSessionResponse, SessionAdditionalDirectoriesCapabilities, SessionCapabilities,
    SessionCloseCapabilities, SessionConfigOption, SessionDeleteCapabilities,
    SessionListCapabilities, SessionNotification, SessionResumeCapabilities, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Dispatch, Handled, Stdio};
use atomcode_capabilities::session::SessionManager;
use atomcode_coding::{CodingAgentConfig, CodingProviderFactory};

use crate::acp::discovery::handle_list_sessions;
use crate::acp::dispatch::{
    handle_new_session, handle_resume_session, replay_entries_to_v1_updates,
};
use crate::acp::engine::EngineConfig;
use crate::acp::options::{
    handle_set_session_config_option, handle_set_session_mode, session_mode_state,
};
use crate::acp::replay::build_replay_entries;
use crate::acp::sessions::{handle_cancel, handle_close_session, handle_delete_session, Sessions};

/// Resolves a model id to a kernel-ready [`CodingAgentConfig`] for provider
/// reloads, or `None` when the id cannot be resolved. Wired by the CLI entry
/// point where the atomcode config + runtime-config machinery lives.
pub type SessionModelResolver = dyn Fn(&str) -> Option<CodingAgentConfig> + Send + Sync;

/// Per-connection shared state for both ACP handler chains.
///
/// Bundles the session table, engine config, provider factory, session
/// config-option catalog, model/effort resolvers, message-id counter and the
/// client elicitation capability flag into one cloneable value that
/// [`build_v1_agent`] and [`crate::acp::v2::build_v2_agent`] both take. This
/// removes the per-handler `Arc::clone` boilerplate at each closure capture
/// site and makes the shared-ownership boundary explicit (one struct, not nine
/// scattered bindings).
#[derive(Clone)]
pub(crate) struct SharedState {
    /// Live sessions keyed by ACP wire id.
    pub sessions: Sessions,
    /// Provider + model config for session spawning (`None` → handler error).
    pub engine: Arc<Option<EngineConfig>>,
    /// Authenticated provider factory; a distinct provider per session.
    pub provider_factory: Option<Arc<dyn CodingProviderFactory>>,
    /// Auto-allow kernel approval requests without round-tripping to the client.
    pub auto_approve: bool,
    /// Initial session config option catalog.
    pub config_options: Arc<Vec<SessionConfigOption>>,
    /// Resolves a model id to the kernel config for provider reloads.
    pub model_resolver: Option<Arc<SessionModelResolver>>,
    /// Resolves a reasoning-effort value to the kernel config.
    pub effort_resolver: Option<Arc<SessionModelResolver>>,
    /// Per-connection message-id counter shared by both chains.
    pub msg_ids: Arc<AtomicU64>,
    /// Whether the client advertised form elicitation during `initialize`.
    pub client_elicitation_form: Arc<AtomicBool>,
}

/// Resolve the configured engine, or fail closed with the standard "run via
/// `atomcode acp`" message when none was injected. Shared by the v1 and v2
/// chains so the no-engine guard lives in exactly one place.
pub(crate) fn require_engine(
    engine: &Arc<Option<EngineConfig>>,
) -> Result<&EngineConfig, agent_client_protocol::Error> {
    engine.as_ref().as_ref().ok_or_else(|| {
        agent_client_protocol::util::internal_error(
            "acp: no engine configured; run via `atomcode acp`",
        )
    })
}

/// The v1 capabilities advertised in `initialize`: the baseline session surface
/// (`list`/`delete`/`close`/`resume`/`additionalDirectories`), prompt image
/// support, `load_session`, and MCP `http` transport. Must stay in lock-step
/// with what the v1 chain actually implements — no `auth`/`fs`/`terminal`
/// advertisement (not implemented), and no MCP `sse` (no SSE transport in the
/// capabilities MCP layer; `http` is the only advertised non-stdio transport).
fn v1_agent_capabilities() -> AgentCapabilities {
    AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(PromptCapabilities::new().image(true))
        .mcp_capabilities(McpCapabilities::new().http(true))
        .session_capabilities(
            SessionCapabilities::new()
                .list(SessionListCapabilities::new())
                .delete(SessionDeleteCapabilities::new())
                .close(SessionCloseCapabilities::new())
                .resume(SessionResumeCapabilities::new())
                .additional_directories(SessionAdditionalDirectoriesCapabilities::new()),
        )
}

/// Options for the ACP stdio server.
///
/// `engine` supplies provider config; `provider_factory` creates a provider for
/// each session so session identity and gateway affinity never leak across ACP
/// sessions.
#[derive(Default)]
pub struct AcpServeOptions {
    /// Provider + model config for session spawning.  `None` → handler returns
    /// an error telling the user to run via `atomcode acp`.
    pub engine: Option<crate::acp::engine::EngineConfig>,
    /// Authenticated provider factory, e.g. the AtomGit gateway factory.
    /// When `None`, the native default factory is used.
    pub provider_factory: Option<Arc<dyn CodingProviderFactory>>,
    /// When `true` (`--dangerously-skip-permissions`), kernel approval requests are
    /// auto-allowed in the turn loop WITHOUT round-tripping to the ACP client.
    pub auto_approve: bool,
    /// Initial session config option catalog. Empty → `session/set_config_option`
    /// is not advertised and errors on use.
    pub session_config_options: Vec<SessionConfigOption>,
    /// Resolves a model id (the `model` select option) to the kernel config for
    /// `session/set_config_option` provider reloads. `None` → model switching
    /// errors on use.
    pub session_model_resolver: Option<Arc<SessionModelResolver>>,
    /// Resolves a reasoning-effort value (`off` / `high` / `max`, the
    /// `reasoning_effort` select option) to the kernel config for
    /// `session/set_config_option` provider reloads. `None` → effort switching
    /// errors on use.
    pub session_effort_resolver: Option<Arc<SessionModelResolver>>,
}

/// Run the ACP agent server on stdin/stdout until the connection closes.
///
/// **stdout is reserved exclusively for the ACP JSON-RPC stream.**
/// All diagnostics must go to stderr.
pub async fn serve_stdio(opts: AcpServeOptions) -> anyhow::Result<()> {
    serve_over(opts, Stdio::new()).await
}

/// Build the fully-wired ACP agent and run it over an arbitrary transport.
///
/// This is the transport-agnostic core that [`serve_stdio`] wraps with
/// [`Stdio`].  The handler wiring (initialize / session·new / session·prompt /
/// session·cancel / fallback dispatch) lives here ONCE; the integration test
/// reuses the exact same wired agent over an in-process
/// [`agent_client_protocol::Channel`] instead of stdio, so the test exercises
/// the real handlers with no subprocess and no network.
///
/// `transport` must connect *to* the [`Agent`] role — `Stdio`, a `Channel`
/// endpoint, etc.  The connection runs until it closes (or the client end is
/// dropped).
pub async fn serve_over<T>(opts: AcpServeOptions, transport: T) -> anyhow::Result<()>
where
    T: ConnectTo<Agent> + 'static,
{
    // One shared-state bundle, handed to both handler chains.
    let state = SharedState {
        sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        engine: Arc::new(opts.engine),
        provider_factory: opts.provider_factory,
        auto_approve: opts.auto_approve,
        config_options: Arc::new(opts.session_config_options),
        model_resolver: opts.session_model_resolver,
        effort_resolver: opts.session_effort_resolver,
        // Per-connection message-id counter, shared by the v1 and v2 chains so
        // message ids never collide across protocol generations.
        msg_ids: Arc::new(AtomicU64::new(0)),
        // Whether the client advertised `clientCapabilities.elicitation.form`
        // during initialize. Captured by the initialize handlers and read by the
        // turn loops: `request_user_input` only maps to `elicitation/create` when
        // the client supports the form mode (protocol MUST: never request a mode
        // the client did not advertise).
        client_elicitation_form: Arc::new(AtomicBool::new(false)),
    };

    // v1 chain (stable): the default handler set. Clients that negotiate v2
    // are routed to the separate v2 chain below.
    let v1_agent = build_v1_agent(state.clone());
    // v2 chain (draft, feature-gated): a distinct handler set. Both chains
    // share the same session table, engine, provider factory, config-option
    // catalog, and message-id counter through the shared bundle.
    let v2_agent = v2::build_v2_agent(state);

    // The router picks v1 or v2 per connection from the client's `initialize`
    // protocol version; the SDK negotiates and rejects mismatches.
    Agent
        .protocol_router()
        .with_v1(v1_agent)
        .with_v2(v2_agent)
        .connect_to(transport)
        .await
        .map_err(|e| anyhow::anyhow!("acp serve failed: {e}"))
}

/// Build the stable v1 handler chain.
///
/// Mirrors [`crate::acp::v2::build_v2_agent`]: both take a [`SharedState`],
/// destructure it into the local bindings the per-handler closures capture,
/// and return a router-compatible `ConnectTo<Client>` agent. The v1 chain is
/// the default handler set.
fn build_v1_agent(state: SharedState) -> impl ConnectTo<Client> + 'static {
    let SharedState {
        sessions,
        engine,
        provider_factory,
        auto_approve,
        config_options,
        model_resolver,
        effort_resolver,
        msg_ids,
        client_elicitation_form,
    } = state;
    Agent
        .builder()
        .name("atomcode")
        .on_receive_request(
            {
                let client_elicitation_form = Arc::clone(&client_elicitation_form);
                async move |init: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                    // Record whether the client supports form elicitation; the
                    // prompt turn loop gates `request_user_input` on it.
                    client_elicitation_form.store(
                        init.client_capabilities
                            .elicitation
                            .as_ref()
                            .and_then(|e| e.form.as_ref())
                            .is_some(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    responder.respond(
                        InitializeResponse::new(init.protocol_version)
                            .agent_info(
                                Implementation::new("atomcode", env!("CARGO_PKG_VERSION"))
                                    .title("AtomCode"),
                            )
                            .agent_capabilities(v1_agent_capabilities()),
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
                    let resp = handle_new_session(
                        engine_ref,
                        provider_factory.clone(),
                        &sessions,
                        req,
                        &config_options,
                    )
                    .await?;
                    let sid = resp.session_id.clone();
                    responder.respond(resp)?;
                    // Advertise the slash-command surface right after setup (the
                    // names/descriptions come from the single built-in command
                    // table). Best-effort: a dropped notification must not fail
                    // the already-accepted session/new — swallow the error so
                    // the handler returns success (the request is already
                    // answered, so a send failure only means the connection is
                    // closing and there is nobody to receive it).
                    let _ = cx.send_notification(SessionNotification::new(
                        sid,
                        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                            crate::acp::commands::available_acp_commands(),
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
                let engine = Arc::clone(&engine);
                let provider_factory = provider_factory.clone();
                let config_options = Arc::clone(&config_options);
                async move |req: ResumeSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let engine_ref = require_engine(&engine)?;
                    // Persisted history: same native catalog as `session/list`.
                    let scan = SessionManager::scan_all();
                    let (mcp_configs, ignored) =
                        mcp::acp_mcp_server_configs(&req.mcp_servers);
                    mcp::log_ignored_mcp_server_names(&ignored);
                    let id = handle_resume_session(
                        engine_ref,
                        provider_factory.clone(),
                        &sessions,
                        &req.session_id,
                        req.cwd.clone(),
                        &config_options,
                        mcp_configs,
                        req.additional_directories.clone(),
                        &scan,
                    )
                    .await?;
                    // v1 resume: silent restore (MUST NOT replay). The response
                    // echoes the initial mode/config state, mirroring
                    // `session/new`.
                    let mut resp = ResumeSessionResponse::new().modes(session_mode_state());
                    if !config_options.is_empty() {
                        resp = resp.config_options(config_options.to_vec());
                    }
                    responder.respond(resp)?;
                    // Best-effort: a dropped notification must not fail the
                    // already-accepted session/resume (same reasoning as the
                    // session/new handler above).
                    let _ = cx.send_notification(SessionNotification::new(
                        id,
                        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                            crate::acp::commands::available_acp_commands(),
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
                let engine = Arc::clone(&engine);
                let provider_factory = provider_factory.clone();
                let config_options = Arc::clone(&config_options);
                let msg_ids = Arc::clone(&msg_ids);
                async move |req: LoadSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let engine_ref = require_engine(&engine)?;
                    let scan = SessionManager::scan_all();
                    let (mcp_configs, ignored) = mcp::acp_mcp_server_configs(&req.mcp_servers);
                    mcp::log_ignored_mcp_server_names(&ignored);
                    // Restore FIRST: replay only makes sense for a session that
                    // actually loaded, and a failed restore must fail closed
                    // before any history is emitted to the client.
                    let id = handle_resume_session(
                        engine_ref,
                        provider_factory.clone(),
                        &sessions,
                        &req.session_id,
                        req.cwd.clone(),
                        &config_options,
                        mcp_configs,
                        req.additional_directories.clone(),
                        &scan,
                    )
                    .await?;
                    // `session/load` MUST replay the full history via
                    // `session/update` before responding (v1 spec). Same native
                    // catalog and display rules as `session/list` / v2 resume.
                    let native_id = sessions::native_id_from_wire(&req.session_id).ok_or_else(|| {
                        agent_client_protocol::util::internal_error(
                            "acp: load replay: invalid session id",
                        )
                    })?;
                    let entries = build_replay_entries(native_id, &req.cwd)
                        .map_err(agent_client_protocol::util::internal_error)?;
                    let updates = replay_entries_to_v1_updates(&entries, &msg_ids);
                    for update in updates {
                        cx.send_notification(SessionNotification::new(id.clone(), update))?;
                    }
                    // Echo initial mode/config state, mirroring `session/resume`.
                    let mut resp = LoadSessionResponse::new().modes(session_mode_state());
                    if !config_options.is_empty() {
                        resp = resp.config_options(config_options.to_vec());
                    }
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: SetSessionModeRequest, responder, cx: ConnectionTo<Client>| {
                    let resp = handle_set_session_mode(&sessions, &cx, &req).await?;
                    responder.respond(resp)
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
                    let resolver = model_resolver.as_deref();
                    let effort = effort_resolver.as_deref();
                    let resp =
                        handle_set_session_config_option(&sessions, &cx, &req, resolver, effort)
                            .await?;
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: CloseSessionRequest, responder, _cx: ConnectionTo<Client>| {
                    let resp = handle_close_session(&sessions, &req.session_id).await;
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: DeleteSessionRequest, responder, _cx: ConnectionTo<Client>| {
                    let scan = SessionManager::scan_all();
                    let resp = handle_delete_session(&sessions, &req.session_id, &scan).await?;
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: ListSessionsRequest, responder, _cx: ConnectionTo<Client>| {
                    let scan = SessionManager::scan_all();
                    let resp = handle_list_sessions(&sessions, &req, &scan).await?;
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let model_resolver = model_resolver.clone();
                let effort_resolver = effort_resolver.clone();
                let turn_msg_ids = Arc::clone(&msg_ids);
                let client_elicitation_form = Arc::clone(&client_elicitation_form);
                async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    // The turn MUST run off the dispatch loop: a handler that
                    // awaited the whole turn inline would block the single-task
                    // loop, so a mid-turn `session/cancel` and the
                    // client's permission responses could never be processed.
                    // Spawn the turn, hand it the deferred `responder`, and
                    // return immediately so the loop stays free.
                    let (text, images, has_attachments) = dispatch::prompt_text(&req);
                    let sid = req.session_id.clone();
                    let sessions = Arc::clone(&sessions);
                    let resolver_arc = model_resolver.clone();
                    let effort_arc = effort_resolver.clone();
                    let elicitation_form_arc = Arc::clone(&client_elicitation_form);
                    cx.spawn({
                        let cx = cx.clone();
                        let msg_ids = Arc::clone(&turn_msg_ids);
                        async move {
                            let resolver = resolver_arc.as_deref();
                            let effort = effort_arc.as_deref();
                            dispatch::run_prompt_turn(
                                cx,
                                sessions,
                                sid,
                                text,
                                images,
                                has_attachments,
                                responder,
                                auto_approve,
                                resolver,
                                effort,
                                msg_ids,
                                elicitation_form_arc.as_ref(),
                            )
                            .await
                        }
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = Arc::clone(&sessions);
                async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                    handle_cancel(&sessions, notif.session_id.0.as_ref()).await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |cancel: CancelRequestNotification, _cx: ConnectionTo<Client>| {
                // Protocol-level `$/cancel_request` (stable v1). The SDK already
                // flips the matching request's cancellation marker before this
                // handler runs (see the cancellation chapter in the SDK docs),
                // and `run_prompt_turn` reacts by cancelling the kernel turn;
                // this handler is the explicit observation point. `request_id`
                // is the id the CLIENT allocated for its own request — e.g. a
                // `session/prompt` it wants to abort.
                eprintln!(
                    "acp: $/cancel_request for request {} (marker flipped; prompt turns cancel their kernel)",
                    cancel.request_id
                );
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, _cx: ConnectionTo<Client>| {
                // Catch-all for messages no typed handler above claimed. CRITICAL: only
                // claim unknown client→agent REQUESTS (reply with an error so the client
                // gets a clean failure, not a hang). RESPONSES and NOTIFICATIONS MUST pass
                // through (`Handled::No`) to the crate's built-in router.
                //
                // Why this matters: this handler receives `Dispatch<UntypedMessage>`, whose
                // `matches_method()` is ALWAYS true, so it sees every message — including the
                // `Dispatch::Response` carrying the client's reply to our outgoing
                // `session/request_permission`. The old code called `respond_with_error` on
                // it, which for a Response forwards the error to the task awaiting it — so
                // `handle_approval`'s `block_task().await` got `Err("unhandled message")` for
                // EVERY approval (even "Allow"), which (before the resilience fix) tore the
                // whole ACP connection down and wiped the client's thread. Passing responses
                // through lets the built-in forwarder deliver them to their awaiter.
                match message {
                    Dispatch::Request(_, responder) => {
                        responder.respond_with_error(
                            agent_client_protocol::util::internal_error("unhandled request"),
                        )?;
                        Ok(Handled::Yes)
                    }
                    _ => Ok(Handled::No {
                        message,
                        retry: false,
                    }),
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_capabilities_only_advertise_implemented_surfaces() {
        // The v1 `initialize` response advertises only what the v1 chain
        // implements: `load_session`, prompt image support, MCP `http` transport,
        // and the baseline session surface (`list`/`delete`/`close`/`resume`/
        // `additionalDirectories`). It must NOT advertise auth (empty
        // authMethods → no authenticate/logout), MCP `sse` (no SSE transport in
        // the capabilities MCP layer), or any client-side fs/terminal surface.
        let caps = v1_agent_capabilities();
        let json = serde_json::to_value(&caps).unwrap();
        assert_eq!(json["loadSession"], true, "load_session advertised");
        assert_eq!(
            json["promptCapabilities"]["image"], true,
            "image advertised"
        );
        let session = json.get("sessionCapabilities").expect("session present");
        assert!(session.get("list").is_some(), "list advertised");
        assert!(session.get("delete").is_some(), "delete advertised");
        assert!(session.get("close").is_some(), "close advertised");
        assert!(session.get("resume").is_some(), "resume advertised");
        assert!(
            session.get("additionalDirectories").is_some(),
            "additionalDirectories advertised"
        );
        // `authenticate`/`logout` are NOT advertised. v1 gates `logout` behind
        // `auth.logout`: an absent/null `logout` means the method is not
        // supported. (`auth` itself is a struct field, always present.)
        let logout = json.get("auth").and_then(|a| a.get("logout"));
        assert!(
            logout.map_or(true, serde_json::Value::is_null),
            "logout must not be advertised: {json}"
        );
        // MCP `http` transport is advertised; `sse` is not (no SSE transport).
        let mcp = json
            .get("mcpCapabilities")
            .expect("mcpCapabilities present");
        assert_eq!(mcp["http"], true, "http MCP transport advertised: {json}");
        assert_eq!(
            mcp["sse"], false,
            "sse MCP transport not advertised: {json}"
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
}
