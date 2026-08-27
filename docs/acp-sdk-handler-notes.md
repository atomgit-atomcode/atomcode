# ACP SDK handler/transport notes (2026-06-29)

Historical spike notes recorded while wiring the `atomcode acp` agent against
`agent-client-protocol`. Kept as reference only — the code is authoritative.

## Handler closure ergonomics

- `responder.respond(response)`:
  `fn respond(self, response: T) -> Result<(), agent_client_protocol::Error>`,
  where `T = Req::Response` for request handlers.

- `on_receive_request!()` expands to
  `|f: &mut _, req, responder, cx| Box::pin(f(req, responder, cx))`
  (needed until return-type notation stabilises; must be passed as final arg).

- `on_receive_dispatch!()` expands to
  `|f: &mut _, dispatch, cx| Box::pin(f(dispatch, cx))`.

- `util::internal_error(message)`:
  `fn internal_error(message: impl ToString) -> agent_client_protocol::Error`
  (calls `Error::internal_error().data(message.to_string())`).

## Dispatch loop concurrency

Single-async-task, non-concurrent by design. Per the crate source comment:
"The connection processes messages on a single async task. While a handler is
running, no other messages can be processed." Handlers block the loop until
they return; for concurrent work use `cx.spawn()`.

## Non-stdio in-memory transport

`agent_client_protocol::Channel` — call `Channel::duplex()` for a
`(Channel, Channel)` pair. Each `Channel` implements `ConnectTo<R>` for any
`Role`, so it works for in-process integration tests without a subprocess.
Re-exported from the crate root (from `jsonrpc`).