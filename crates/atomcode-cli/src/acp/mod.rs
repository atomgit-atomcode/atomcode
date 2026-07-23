// SPIKE FINDINGS (Task 1, 2026-06-29):
//
// 1. Handler closure ergonomics (actual signatures confirmed by source + compile):
//
//    responder.respond(response):
//      fn respond(self, response: T) -> Result<(), agent_client_protocol::Error>
//      where T = Req::Response for request handlers
//
//    on_receive_request!() macro expands to:
//      |f: &mut _, req, responder, cx| Box::pin(f(req, responder, cx))
//      (needed until return-type notation stabilises; must always be passed as final arg)
//
//    on_receive_dispatch!() macro expands to:
//      |f: &mut _, dispatch, cx| Box::pin(f(dispatch, cx))
//
//    util::internal_error(message):
//      fn internal_error(message: impl ToString) -> agent_client_protocol::Error
//      (calls Error::internal_error().data(message.to_string()))
//
// 2. Dispatch loop concurrency:
//    Single-async-task, non-concurrent by design. From the crate source comment:
//    "The connection processes messages on a single async task. While a handler
//    is running, no other messages can be processed." Handlers block the loop
//    until they return; for concurrent work, callers must use cx.spawn().
//
// 3. Non-Stdio in-memory transport:
//    agent_client_protocol::Channel — call Channel::duplex() to get a (Channel, Channel) pair.
//    Each Channel implements ConnectTo<R> for any Role, making it fully usable
//    for in-process integration tests without spawning a binary.
//    Exposed publicly in the crate root (re-exported from jsonrpc).

pub mod dispatch;
pub mod engine;
pub mod permission;
pub mod translate;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, InitializeRequest, InitializeResponse,
    NewSessionRequest, PromptCapabilities, PromptRequest,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Dispatch, Handled, Stdio};
use atomcode_coding::CodingProviderFactory;

use crate::acp::dispatch::{handle_cancel, handle_new_session, Sessions};

/// Options for the ACP stdio server.
///
/// `engine` supplies provider config; `provider_factory` creates a provider for
/// each session so session identity and gateway affinity never leak across ACP
/// sessions.
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
}

impl Default for AcpServeOptions {
    fn default() -> Self {
        Self {
            engine: None,
            provider_factory: None,
            auto_approve: false,
        }
    }
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
/// (Task 11) reuses the exact same wired agent over an in-process
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
    // Shared state for all session handlers (Tasks 6-9).
    let sessions: Sessions = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let counter = Arc::new(AtomicU64::new(0));
    let engine = Arc::new(opts.engine);
    let provider_factory = opts.provider_factory;
    let auto_approve = opts.auto_approve;

    Agent
        .builder()
        .name("atomcode")
        .on_receive_request(
            async move |init: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(init.protocol_version).agent_capabilities(
                        AgentCapabilities::new()
                            .load_session(false)
                            .prompt_capabilities(PromptCapabilities::new().image(true)),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let counter = Arc::clone(&counter);
                let engine = Arc::clone(&engine);
                let provider_factory = provider_factory.clone();
                async move |req: NewSessionRequest, responder, _cx: ConnectionTo<Client>| {
                    let engine_ref = engine.as_ref().as_ref().ok_or_else(|| {
                        agent_client_protocol::util::internal_error(
                            "acp: no engine configured; run via `atomcode acp`",
                        )
                    })?;
                    let resp = handle_new_session(
                        engine_ref,
                        provider_factory.clone(),
                        &sessions,
                        &counter,
                        req,
                    )
                    .await?;
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    // The turn MUST run off the dispatch loop: a handler that
                    // awaited the whole turn inline would block the single-task
                    // loop, so a mid-turn `session/cancel` (Task 9) and the
                    // client's permission responses could never be processed.
                    // Spawn the turn, hand it the deferred `responder`, and
                    // return immediately so the loop stays free.
                    let (text, images) = dispatch::prompt_text(&req);
                    let sid = req.session_id.clone();
                    let sessions = Arc::clone(&sessions);
                    cx.spawn({
                        let cx = cx.clone();
                        async move {
                            dispatch::run_prompt_turn(
                                cx,
                                sessions,
                                sid,
                                text,
                                images,
                                responder,
                                auto_approve,
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
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
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
                if matches!(message, Dispatch::Request(..)) {
                    message.respond_with_error(
                        agent_client_protocol::util::internal_error("unhandled request"),
                        cx,
                    )?;
                    Ok(Handled::Yes)
                } else {
                    Ok(Handled::No {
                        message,
                        retry: false,
                    })
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(transport)
        .await
        .map_err(|e| anyhow::anyhow!("acp serve failed: {e}"))
}
