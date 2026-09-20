//! Protocol-neutral prompt turn driver, shared by the v1 and v2 chains.
//!
//! The two ACP chains previously duplicated the entire turn loop
//! (`run_prompt_turn` in `dispatch.rs` vs `run_prompt_turn_v2` in `v2.rs`):
//! the same event-loop skeleton — session resolution, `submit`, the
//! `$/cancel_request` watcher, approval/elicitation/unknown-request handling,
//! todo/plan bookkeeping, `Usage` message-id advancement, auto-title, and the
//! drain-`Error`-then-`TurnFinished` discipline — was written twice with only
//! the wire shapes differing. This module owns that skeleton once.
//!
//! [`TurnWire`] is the per-protocol surface the driver needs: each chain
//! implements it as a thin struct holding its own `cx`/session-id/responder,
//! translating neutral kernel events to its own `session/update` wire shape.
//! The driver is deliberately protocol-agnostic — it never mentions v1 or v2
//! schema types, session ids, or responders.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use agent_client_protocol::{Client, ConnectionTo, RequestCancellation};
use atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND;
use atomcode_capabilities::tools::todo::{reduce_todos, TodoItem};
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::message::ImageContent;
use atomcode_kernel::tool::ToolCall;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use crate::acp::sessions::{derive_title, next_message_id, Sessions};

/// The per-protocol surface a prompt turn needs from its chain.
///
/// Implementations are cheap per-turn value structs: v1 holds its
/// `ConnectionTo<Client>` + v1 `SessionId` + the deferred `Responder`; v2 holds
/// the same trio in v2 types. The driver calls these methods in the shared
/// event loop; everything chain-specific lives here.
pub(crate) trait TurnWire {
    /// The chain's own `session/update` value type (v1 vs v2 schema).
    type Update: Send;

    /// Send one update to the client, wrapped in the chain's session
    /// notification type. Failures propagate (a dead transport tears the
    /// connection — reserved for genuine transport death, per the loops' rule).
    fn notify(&self, update: Self::Update) -> Result<(), agent_client_protocol::Error>;

    /// The optional `running` state update the chain emits when the turn
    /// starts (v2: `state_update(running)`; v1: none — its response is the
    /// ack).
    fn running_update(&self) -> Option<Self::Update>;

    /// Intercept a local slash command before the kernel submission. Returns
    /// `true` when the command was handled: the chain replied via its own
    /// notification/response (using `msg_id` for any chunk) and the turn ends
    /// without a kernel round-trip. Returns `false` when the prompt must reach
    /// the kernel whole (not a slash command, an unknown `/…`, or attachment
    /// prompts — a local handler would silently drop attachments).
    /// v1 executes its slash command table locally; v2 sends slash input to
    /// the kernel and never intercepts.
    async fn try_slash(
        &mut self,
        text: &str,
        has_attachments: bool,
        msg_id: &str,
    ) -> Result<bool, agent_client_protocol::Error>;

    /// Map one neutral kernel `AgentEvent` to a wire update (or skip it).
    /// `msg_id` is the current LLM round's message id (a plain string; the
    /// chain wraps it as its own `MessageId` / optional id).
    fn translate(&self, ev: &AgentEvent, msg_id: &str) -> Option<Self::Update>;

    /// Handle a kernel approval request end to end: announce the tool call as
    /// `pending` (chain-specific shape), then auto-allow or round-trip
    /// `session/request_permission`, fail-closed to deny on any error. Never
    /// propagates a permission-round-trip failure (the turn keeps going);
    /// only genuine transport failures during the announcement propagate.
    async fn handle_approval_request(
        &mut self,
        commands: &mpsc::UnboundedSender<AgentCommand>,
        req_id: u64,
        payload: serde_json::Value,
        auto_approve: bool,
    ) -> Result<(), agent_client_protocol::Error>;

    /// Handle a kernel `request_user_input` request: map to the chain's
    /// `elicitation/create` round-trip (form), answered fail-closed. Never
    /// propagates an error (same rule as approval).
    async fn handle_user_input_request(
        &mut self,
        commands: &mpsc::UnboundedSender<AgentCommand>,
        req_id: u64,
        payload: serde_json::Value,
        form_supported: bool,
    );

    /// Transition a tool call that the approval round-trip already announced
    /// as `pending` to `in_progress`, returning the update to emit. Chains
    /// whose tool updates are upserts (v2) return `None` and let the generic
    /// `translate` create the record; v1's pending announcement needs an
    /// explicit `tool_call_update(in_progress)` instead of a duplicate
    /// `tool_call` record.
    fn on_tool_started(&mut self, call: &ToolCall) -> Option<Self::Update>;

    /// Build the plan / plan_update from the session's derived todos.
    fn plan_update(&self, todos: &[TodoItem], native_id: &str) -> Self::Update;

    /// Build the `session_info_update` carrying the auto-derived session title.
    fn session_info_update(&self, title: &str) -> Self::Update;

    /// Terminal response for a prompt on an unknown session:
    /// v1 answers the deferred responder with an internal error; v2 emits
    /// idle(`other`). No message id exists yet — the session lookup precedes
    /// id allocation.
    fn unknown_session(&mut self) -> Result<(), agent_client_protocol::Error>;

    /// Terminal response when the kernel agent died before/during submit:
    /// v1 answers the responder with an internal error; v2 emits a message
    /// chunk `msg_id` + idle(`other`).
    fn kernel_dead(&mut self, msg_id: &str) -> Result<(), agent_client_protocol::Error>;

    /// Map the final terminal to the chain's response: v1 answers the deferred
    /// responder with a `PromptResponse` carrying the mapped stop reason (or an
    /// internal error when no usable stop reason exists); v2 emits a message
    /// chunk for the last error (if any) then idle with the mapped reason.
    ///
    /// `Ok` is the turn's own terminal, `Err` is the loop giving up on it (the
    /// stream ended first). Both carry a reason now: under the contract a turn
    /// completes with a reason and nothing else — whether its record was
    /// written is a separate fact the host pushes
    /// (`HostEvent::PersistenceFailed`), which is why this no longer takes the
    /// product's completion enum.
    fn finish(
        &mut self,
        terminal: Result<StopReason, StopReason>,
        last_error: Option<String>,
        msg_id: &str,
    ) -> Result<(), agent_client_protocol::Error>;
}

/// Drive one prompt turn against a live runtime, protocol-neutrally.
///
/// This is the shared skeleton the v1 and v2 chains used to duplicate. It
/// resolves the session, submits the user input, installs the
/// `$/cancel_request` watcher, runs the event loop (approval / elicitation /
/// unknown kernel requests fail closed; `AgentError` is drained so its
/// trailing `TurnComplete` cannot poison the next turn; `Usage` advances the
/// message id and accumulates session usage), derives the auto-title, and maps
/// the terminal through [`TurnWire::finish`]. All wire shapes — notifications,
/// event translation, approval/elicitation round-trips, plan/title updates,
/// and final response — are delegated to `wire`.
/// Message shape is per-chain, so slash interception is a per-chain hook
/// too: [`TurnWire::try_slash`] runs before the kernel submission, after the
/// turn's message id is allocated — v1 executes its local slash command table
/// and answers the turn itself; v2 sends slash input to the kernel and never
/// intercepts.
///
/// # Error discipline
///
/// The `cx.send_notification` failures propagate (transport death tears the
/// whole connection — the established rule); approval/elicitation round-trip
/// failures never do (single-call events; fail closed and keep the turn).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn<W: TurnWire>(
    wire: &mut W,
    cx: ConnectionTo<Client>,
    sessions: &Sessions,
    sid: &str,
    text: String,
    images: Vec<ImageContent>,
    has_attachments: bool,
    auto_approve: bool,
    msg_ids: &Arc<AtomicU64>,
    elicitation_form: &AtomicBool,
    cancellation: RequestCancellation,
) -> Result<(), agent_client_protocol::Error> {
    // Take what the turn needs (clonable command sender + the events mutex
    // Arc), then release the map lock so it is never held across the turn.
    // The message id is allocated AFTER the lookup: an unknown-session prompt
    // never consumes an id (v2 already ordered it this way).
    #[allow(clippy::type_complexity)]
    let (commands, events, catalog, native): (
        mpsc::UnboundedSender<AgentCommand>,
        Arc<Mutex<mpsc::UnboundedReceiver<AgentEvent>>>,
        Arc<Mutex<Vec<atomcode_kernel::agent::CommandDescription>>>,
        String,
    ) = {
        let map = sessions.lock().await;
        match map.get(sid) {
            Some(st) => (
                st.commands.clone(),
                Arc::clone(&st.events),
                Arc::clone(&st.catalog),
                st.native_id.clone(),
            ),
            None => return wire.unknown_session(),
        }
    };
    // v2 announces `running` state before submit; v1 emits nothing here.
    if let Some(update) = wire.running_update() {
        wire.notify(update)?;
    }

    // Each `Usage` closes one LLM output round: the next text/thought delta
    // starts a new message (same id groups one message's chunks; a changed id
    // opens a new one). `usage_update` carries no messageId, so advancing here
    // is side-effect free for that event.
    let mut msg_id = next_message_id(msg_ids);

    // Local slash commands run before locking the events receiver, so command
    // handlers are free to touch the session table, and an intercepted slash
    // ends the turn without a kernel round-trip. Only attachment-free prompts
    // are eligible: a prompt that carries images/resources must reach the
    // kernel whole (a local handler would silently drop the attachments).
    if wire.try_slash(&text, has_attachments, &msg_id).await? {
        return Ok(());
    }

    // A command the **agent** registered — goal, review, worktree, stopping a
    // member. Not a prompt: it runs in the agent and answers, and only
    // sometimes leaves the model work to do. Never one when the prompt carries
    // attachments, for the reason a local slash is not: a handler would drop
    // what was attached.
    let invoked = if has_attachments {
        None
    } else {
        let names = catalog.lock().await.clone();
        crate::acp::commands::parse_catalog_command(&text, &names)
            .map(|(name, args)| (name, args.to_string()))
    };

    // Lock the receiver BEFORE enqueuing this turn's message: one prompt runs
    // per session at a time, so a concurrent same-session prompt blocks here
    // on the events mutex and cannot interleave its `SendMessage` into the
    // kernel ahead of this turn's recv loop.
    let mut rx = events.lock().await;
    // The id is the receipt: the `Invoked` that carries the result names it, so
    // a late one from an earlier command cannot be taken for this one's.
    let invoke_id = invoked
        .as_ref()
        .map(|_| format!("acp-{}", next_message_id(msg_ids)));
    let submitted = match (&invoked, &invoke_id) {
        (Some((name, args)), Some(id)) => commands.send(AgentCommand::Invoke {
            id: id.clone().into(),
            // The session it acts on, the same way the screen names it: a
            // command addresses an agent, and an empty name addresses none.
            session: native.clone(),
            name: name.clone(),
            args: args.clone(),
        }),
        _ => commands.send(AgentCommand::SendMessage {
            text: text.clone(),
            images,
        }),
    };
    if submitted.is_err() {
        // The kernel agent is gone (panicked / cancelled / session torn down):
        // the prompt will NEVER reach it, so answer the terminal now instead of
        // falling into the recv loop and reporting a FALSE end_turn success.
        // Drop the receiver so the chain's terminal (which may answer a
        // responder synchronously) is not racing a held events lock.
        drop(rx);
        return wire.kernel_dead(&msg_id);
    }

    // Protocol-level `$/cancel_request` support: the SDK flips the prompt
    // request's cancellation marker; a watcher task reacts by cancelling the
    // kernel turn (same effect as `session/cancel`). The kernel emits a
    // Cancelled terminal, the loop drains it, and the chain reports its
    // `cancelled` stop reason — never an error. The watcher exits via
    // `done_tx` when the turn ends, so it never leaks into a later turn.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let cancel_commands = commands.clone();
    cx.spawn(async move {
        tokio::select! {
            _ = cancellation.cancelled() => {
                let _ = cancel_commands.send(AgentCommand::Cancel);
            }
            _ = done_rx => {}
        }
        Ok(())
    })?;

    // The kernel ALWAYS emits a trailing `TurnComplete` after an `Error` (see
    // kernel `finish_turn`). We must DRAIN that `TurnComplete` rather than
    // return on the `Error` — otherwise it stays buffered in the session's
    // events channel and poisons the NEXT prompt.
    let mut last_error: Option<String> = None;
    // Tool call id → (name, args) for `todowrite`/`todo`, so a completed
    // result can fold its arguments into the session's todo history.
    let mut todo_started: HashMap<String, (String, String)> = HashMap::new();

    let terminal = loop {
        match rx.recv().await {
            Some(AgentEvent::Request { id, kind, payload }) if kind == "approval" => {
                wire.handle_approval_request(&commands, id, payload, auto_approve)
                    .await?;
            }
            Some(AgentEvent::Request { id, kind, payload }) if kind == REQUEST_USER_INPUT_KIND => {
                wire.handle_user_input_request(
                    &commands,
                    id,
                    payload,
                    elicitation_form.load(Ordering::Relaxed),
                )
                .await;
            }
            Some(AgentEvent::Request { id, .. }) => {
                // Unknown (non-approval) kernel request kind: we cannot satisfy
                // it. Respond with null (fail-closed) so the kernel unparks and
                // the turn cannot hang waiting for a reply we will never
                // produce.
                eprintln!("acp: unhandled kernel request kind; responding null");
                let _ = commands.send(AgentCommand::Respond {
                    id,
                    value: serde_json::Value::Null,
                });
            }
            // What the agent offers can change mid-session — a row mounted by a
            // reload, a member that came or went. This is the only place this
            // channel reads the stream, so it is where the catalog is kept
            // current.
            Some(AgentEvent::Described { description }) => {
                *catalog.lock().await = description.commands;
            }
            // The command answered. Whether this exchange is over depends on
            // what it left behind: one that only answered is done here; one
            // that queued a message has a turn starting, and that turn's facts
            // belong to this request rather than to the next one.
            Some(AgentEvent::Invoked { id, output, queued })
                if invoke_id.as_deref() == Some(id.as_ref()) =>
            {
                if let Some(update) = wire.translate(
                    &AgentEvent::Invoked {
                        id: id.clone(),
                        output,
                        queued,
                    },
                    &msg_id,
                ) {
                    wire.notify(update)?;
                }
                if !queued {
                    break Ok(StopReason::Stopped);
                }
            }
            Some(AgentEvent::TurnComplete { reason, .. }) => break Ok(reason),
            Some(AgentEvent::Error { message, .. }) => {
                // Do NOT break — keep looping so the trailing `TurnComplete`
                // is consumed and cannot poison the next turn on this session.
                last_error = Some(message);
            }
            Some(ev) => {
                // Todo/plan bookkeeping before the generic translation: a
                // completed `todowrite`/`todo` call mutates the session's
                // derived todo state and re-emits the full plan update
                // (clients replace the whole plan per update).
                match &ev {
                    AgentEvent::ToolStarted { call }
                        if matches!(call.name.as_str(), "todowrite" | "todo") =>
                    {
                        todo_started
                            .insert(call.id.clone(), (call.name.clone(), call.arguments.clone()));
                    }
                    AgentEvent::ToolResult { result } => {
                        if let Some((name, args)) = todo_started.remove(&result.call_id) {
                            if !result.is_error {
                                let update = {
                                    let mut map = sessions.lock().await;
                                    match map.get_mut(sid) {
                                        Some(state) => {
                                            state.todo_calls.push((name, args));
                                            let todos = reduce_todos(
                                                state
                                                    .todo_calls
                                                    .iter()
                                                    .map(|(n, a)| (n.as_str(), a.as_str())),
                                            );
                                            Some(wire.plan_update(&todos, &state.native_id))
                                        }
                                        None => None,
                                    }
                                };
                                if let Some(update) = update {
                                    wire.notify(update)?;
                                }
                            }
                        }
                    }
                    AgentEvent::Usage(meta) => {
                        msg_id = next_message_id(msg_ids);
                        let mut map = sessions.lock().await;
                        if let Some(state) = map.get_mut(sid) {
                            // Both chains accumulate session usage here (the
                            // v1 `/usage` text and the shared catalog both read
                            // it; v2 previously skipped the accumulation — the
                            // driver unifies it).
                            state.usage.0 += u64::from(meta.tokens.prompt);
                            state.usage.1 += u64::from(meta.tokens.completion);
                        }
                    }
                    _ => {}
                }
                // A tool call the approval round-trip announced as `pending`
                // transitions to `in_progress` instead of creating a duplicate
                // record via the generic translation path.
                if let AgentEvent::ToolStarted { call } = &ev {
                    if let Some(update) = wire.on_tool_started(call) {
                        wire.notify(update)?;
                        continue;
                    }
                }
                if let Some(update) = wire.translate(&ev, &msg_id) {
                    wire.notify(update)?;
                }
            }
            None => {
                last_error =
                    Some("acp: coding runtime event stream closed before turn terminal".into());
                break Err(StopReason::ProviderError);
            }
        }
    };

    // TurnFinished is authoritative. Budget/loop fuses may emit an AgentError
    // diagnostic immediately before their typed terminal; the chain must still
    // map to its own typed terminal instead of misreporting an internal
    // provider failure. Stop the `$/cancel_request` watcher now — a late
    // cancellation must not leak into the next turn on this session.
    let _ = done_tx.send(());
    // Auto-title: once per session, derive a display title from the first real
    // user prompt and broadcast it. Slash turns return before reaching here
    // (handled in the v1 wrapper), so the text arriving here is a real user
    // message (or an unknown `/…` that fell through to the kernel);
    // attachment-only turns have empty text and never title the session.
    // A command is not a prompt, so it never names the session: `/goal ship it`
    // is what somebody asked the agent to do, not what the conversation is.
    if let Some(title) = derive_title(&text).filter(|_| invoked.is_none()) {
        let mut map = sessions.lock().await;
        let announce = match map.get_mut(sid) {
            Some(state) if state.title.is_none() => {
                state.title = Some(title.clone());
                Some(title)
            }
            _ => None,
        };
        drop(map);
        if let Some(title) = announce {
            // Best-effort: a dropped notification must not fail the turn's
            // final response.
            let _ = wire.notify(wire.session_info_update(&title));
        }
    }
    // "The turn finished but its record did not" reaches here as a fact the
    // host pushed, not as a variant of the terminal — the turn did finish. It
    // is reported once and cleared, so the next turn on this session does not
    // inherit it.
    let persisted = {
        let map = sessions.lock().await;
        match map.get(sid) {
            Some(state) => state.persistence_failure.lock().await.take(),
            None => None,
        }
    };
    let last_error = fold_persistence_failure(last_error, persisted);
    wire.finish(terminal, last_error, &msg_id)
}

/// Fold "the turn finished but its record did not" into what the turn reports.
///
/// It used to be a variant of the terminal, and the v1 chain turned it into a
/// protocol error: a turn that had run, produced output and stopped normally
/// was reported to the client as a failure. Under the contract the terminal
/// carries a reason and nothing else — the turn did finish — and the store's
/// trouble arrives separately (`HostEvent::PersistenceFailed`). So it is said
/// rather than thrown: the client hears its normal stop reason, and the text
/// tells the person their turn was not written down.
fn fold_persistence_failure(
    last_error: Option<String>,
    persisted: Option<String>,
) -> Option<String> {
    match persisted {
        None => last_error,
        Some(why) => {
            let said = atomcode_config::i18n::t(atomcode_config::i18n::Msg::TurnNotStoredCli {
                why: &why,
            });
            Some(match last_error {
                Some(had) => format!("{had}\n{said}"),
                None => said.into_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fold_persistence_failure;

    /// A turn that ran and a turn that was written down are two facts.
    ///
    /// Before 6.3 the second one made the first one a protocol error — a client
    /// was told the prompt failed when it had not. Now the turn keeps its own
    /// terminal and the store's trouble is said alongside whatever else went
    /// wrong, never instead of it.
    #[test]
    fn a_turn_that_was_not_written_down_still_finished() {
        use atomcode_config::i18n::{t, Msg};
        // The sentence comes from the table, so this asserts the *composition*
        // and not one language's wording — the same test then holds whichever
        // language the process is in.
        let said = t(Msg::TurnNotStoredCli {
            why: "磁盘满了"
        })
        .into_owned();

        assert_eq!(fold_persistence_failure(None, None), None);
        // Nothing else went wrong: the person still hears about the store.
        assert_eq!(
            fold_persistence_failure(None, Some("磁盘满了".into())).as_deref(),
            Some(said.as_str())
        );
        // Something else did: both, in that order — the turn's own trouble
        // first, because that is what the person was watching.
        let both = fold_persistence_failure(Some("模型断了".into()), Some("磁盘满了".into()));
        assert_eq!(both.as_deref(), Some(format!("模型断了\n{said}").as_str()));
    }
}
