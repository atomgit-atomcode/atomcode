// SPIKE FINDINGS (Task 1, 2026-06-29):
//
// 1. Handler closure ergonomics (actual signatures confirmed by source + compile):
//
//    responder.respond(response):
//      fn respond(self, response: T) -> Result<(), crate::Error>
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
//      fn internal_error(message: impl ToString) -> crate::Error
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
pub mod translate;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, InitializeRequest, InitializeResponse, NewSessionRequest,
    PromptCapabilities,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Stdio};
use atomcode_kernel::provider::LlmProvider;

use crate::dispatch::{handle_new_session, Sessions};

/// Options for the ACP stdio server.
///
/// `engine` supplies provider config; `provider` is the pre-built (signed)
/// provider for AtomGit gateway sessions.  The CLI (Task 10) sets both from the
/// active user config; integration tests (Task 11) inject a stub provider and
/// can leave `engine` as `None` if `provider` is `Some`.
pub struct AcpServeOptions {
    /// Provider + model config for session spawning.  `None` → handler returns
    /// an error telling the user to run via `atomcode acp`.
    pub engine: Option<crate::engine::EngineConfig>,
    /// Pre-built (authenticated) provider, e.g. the AtomGit gateway signer.
    /// When `Some`, forwarded to each `spawn_session` call verbatim.
    /// When `None`, `engine::build_provider` builds a fallback per session.
    pub provider: Option<Arc<dyn LlmProvider>>,
}

impl Default for AcpServeOptions {
    fn default() -> Self {
        Self {
            engine: None,
            provider: None,
        }
    }
}

/// Run the ACP agent server on stdin/stdout until the connection closes.
///
/// **stdout is reserved exclusively for the ACP JSON-RPC stream.**
/// All diagnostics must go to stderr.
pub async fn serve_stdio(opts: AcpServeOptions) -> anyhow::Result<()> {
    // Shared state for all session handlers (Tasks 6-9).
    let sessions: Sessions = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let counter = Arc::new(AtomicU64::new(0));
    let engine = Arc::new(opts.engine);
    let provider = opts.provider;

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
                let provider = provider.clone();
                async move |req: NewSessionRequest, responder, _cx: ConnectionTo<Client>| {
                    let engine_ref = engine.as_ref().as_ref().ok_or_else(|| {
                        agent_client_protocol::util::internal_error(
                            "acp: no engine configured; run via `atomcode acp`",
                        )
                    })?;
                    let resp = handle_new_session(
                        engine_ref,
                        provider.clone(),
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
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled message"),
                    cx,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
        .map_err(|e| anyhow::anyhow!("acp serve failed: {e}"))
}
