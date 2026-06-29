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

pub mod engine;
pub mod translate;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, InitializeRequest, InitializeResponse, PromptCapabilities,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Stdio};

/// Options for the ACP stdio server.
///
/// Fields grow in later tasks as session dispatch, provider selection, and
/// model configuration are wired in.
#[derive(Debug, Clone, Default)]
pub struct AcpServeOptions {
    /// Override the provider name (e.g. "openai", "anthropic").  `None` → use
    /// the globally configured default.
    pub provider: Option<String>,
    /// Override the model name.  `None` → use the provider default.
    pub model: Option<String>,
}

/// Run the ACP agent server on stdin/stdout until the connection closes.
///
/// **stdout is reserved exclusively for the ACP JSON-RPC stream.**
/// All diagnostics must go to stderr.
pub async fn serve_stdio(_opts: AcpServeOptions) -> anyhow::Result<()> {
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
