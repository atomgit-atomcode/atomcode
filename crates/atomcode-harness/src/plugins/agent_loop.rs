//! The turn driver — a plugin like any other.
//!
//! It resolves `llm`, `tools`, `system-prompt` and `sessions` by name, and it
//! dispatches `agent/request` and `tools/execute` as waterfalls. What it does
//! **not** contain is a single branch for approval, argument repair, output
//! capping, persistence, tracing or telemetry: those are listeners other rows
//! install. Deleting this row and providing `agent-loop` from a different plugin
//! replaces the loop without touching anything else in the tree.
//!
//! Everything the model will see is appended to the session log *before* the
//! request is assembled, and the request is assembled *from* the log
//! ([`SessionLog::derive_messages`]). There is no second list.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::tool::{ToolCall, ToolDef, ToolResult};
use atomcode_plexus::{Context, Plugin};
use futures::future::BoxFuture;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::agent::{Agent, MessageOrigin};
use crate::events::{
    AgentInfo, AgentRequest, AssistantChunk, AssistantMessage, Chunk, InboxInserted, ModelRequest,
    ModelResponse, PreStep, RequestError, SessionEventCommitted, StepDecision, ToolBatch, ToolExec,
    ToolResultEvent, ToolsExecuteBatch, TurnEnd, TurnFinishing, TurnProgress, TurnStart,
    TurnStarted, TurnStopping,
};
use crate::seams::{
    AgentLoop, AgentLoopSvc, LlmSvc, SessionProjectionsSvc, StopReason, SystemPromptSvc, ToolsSvc,
    TurnOutcome,
};
use crate::session::{Committed, HeaderReason, InjectionOrigin, SeqNo, SessionEvent, SessionLog};

#[derive(Debug, Deserialize)]
struct LoopRow {
    /// The runaway fuse, not a policy. A tree with no `agent/turn-stopping`
    /// listener still terminates; a tree that has one should hit that first.
    #[serde(default = "default_max_rounds")]
    max_rounds: u32,
    #[serde(default)]
    working_dir: Option<String>,
    /// Check, every round, that nothing reached the model without being logged.
    /// On by default: the invariant is cheap and the failure it catches (a side
    /// channel into the prompt) is invisible otherwise.
    #[serde(default = "default_true")]
    verify_log_invariant: bool,
    /// When a person cancels a turn, take the turn's prompt and partial work out
    /// of what the model sees next (a note of the interruption stays). Off keeps
    /// them, with a cancelled result for any call that never ran.
    #[serde(default)]
    undo_cancelled: bool,
    /// How long a stream may go without an event — first token included —
    /// before the request is abandoned as stalled. `0` waits as long as the
    /// provider does. A stall is retryable: what happens next is the retry
    /// policy's, and a turn that runs out of retries ends as timed out.
    #[serde(default)]
    stream_idle_ms: u64,
}

impl Default for LoopRow {
    fn default() -> Self {
        Self {
            max_rounds: default_max_rounds(),
            working_dir: None,
            verify_log_invariant: true,
            undo_cancelled: false,
            stream_idle_ms: 0,
        }
    }
}

/// The code a stalled stream's error carries, so the loop can tell a turn that
/// timed out from one the provider failed.
pub const STREAM_IDLE_CODE: &str = "stream_idle_timeout";

fn default_max_rounds() -> u32 {
    100
}

fn default_true() -> bool {
    true
}

struct PluginAgentLoop {
    /// The loop's own context: how it reaches services and fires events. Cloned
    /// from the fiber's, so anything it registers still unloads with the row.
    ctx: Context,
    working_dir: PathBuf,
    max_rounds: u32,
    verify_log_invariant: bool,
    undo_cancelled: bool,
    stream_idle: Option<std::time::Duration>,
}

impl PluginAgentLoop {
    /// Commit one fact: append, broadcast, advance projections.
    ///
    /// The single write path. A plugin that wants to react to session state
    /// listens to `session/event`; it never has to be called by the loop.
    fn commit(&self, session: &SessionLog, event: SessionEvent) -> SeqNo {
        let seq = session.append(event.clone());
        self.ctx.emit::<SessionEventCommitted>(&Committed {
            session: session.id().to_string(),
            seq,
            event,
        });
        if let Some(projections) = self.ctx.service::<SessionProjectionsSvc>() {
            projections.advance(session);
        }
        seq
    }

    /// Assemble what the model sees this round: the system prompt from the
    /// fragment registry, the conversation from the log.
    fn assemble(&self, session: &SessionLog, ctx: &Context) -> (Vec<Message>, Vec<ToolDef>) {
        let mut messages = Vec::new();
        if let Some(prompts) = ctx.service::<SystemPromptSvc>() {
            let system = prompts.render();
            if !system.is_empty() {
                messages.push(Message::system(system));
            }
        }
        messages.extend(session.derive_messages());
        let tools = ctx
            .service::<ToolsSvc>()
            .map(|t| t.defs())
            .unwrap_or_default();
        dump_request(session, &messages, &tools);
        (messages, tools)
    }

    /// One model call, wrapped in the `agent/request` waterfall. Listeners see
    /// the assembled request and may rewrite it, wrap the answer, or return one
    /// of their own without ever reaching the provider.
    async fn request(
        &self,
        provider: Arc<dyn LlmProvider>,
        session: Arc<SessionLog>,
        agent_ctx: &Context,
        turn: u64,
        round: u32,
        mut req: ModelRequest,
    ) -> Result<ModelResponse, RequestError> {
        let ctx = agent_ctx.clone();
        agent_ctx
            .waterfall::<AgentRequest, _>(&mut req, move |req| {
                let provider = provider.clone();
                let ctx = ctx.clone();
                let session = session.clone();
                let messages = req.messages.clone();
                let tools = req.tools.clone();
                let options = req.options.clone();
                let idle = self.stream_idle;
                Box::pin(async move {
                    stream_once(
                        &*provider, &ctx, &session, turn, round, messages, tools, options, idle,
                    )
                    .await
                }) as BoxFuture<'_, Result<ModelResponse, RequestError>>
            })
            .await
    }

    /// One round's tool calls, wrapped in the `tools/execute-batch` waterfall.
    ///
    /// The terminal runs them one at a time. Overlapping them is a listener's
    /// job (`tool-exec-parallel`), so a tree with no scheduler mounted still
    /// works — just serially. That is the difference between a default and a
    /// hole.
    async fn execute_batch(
        &self,
        agent_ctx: &Context,
        agent: &Agent,
        calls: Vec<ToolCall>,
        turn: u64,
        step: u32,
    ) -> Vec<ToolResult> {
        let mut batch = ToolBatch {
            calls,
            turn,
            step,
            // An agent given a world of its own works there; the row's
            // directory is the default for the ones that were not.
            working_dir: agent
                .cwd()
                .cloned()
                .unwrap_or_else(|| self.working_dir.clone()),
            cancel: agent.cancel_token(),
        };
        let ctx = agent_ctx.clone();
        agent_ctx
            .waterfall::<ToolsExecuteBatch, _>(&mut batch, move |batch| {
                let ctx = ctx.clone();
                let cancel = batch.cancel.clone();
                let working_dir = batch.working_dir.clone();
                let turn = batch.turn;
                let step = batch.step;
                let calls = batch.calls.clone();
                Box::pin(async move {
                    let mut out = Vec::with_capacity(calls.len());
                    for call in calls {
                        // The same guarantee the parallel scheduler makes: a
                        // call that had not started when the turn was cancelled
                        // never starts. Without it, `/stop` during a four-call
                        // round still runs all four.
                        if cancel.is_cancelled() {
                            out.push(crate::exec::error_result(
                                &call.id,
                                "(cancelled before it started)",
                            ));
                            continue;
                        }
                        out.push(
                            crate::exec::execute_one(
                                &ctx,
                                cancel.clone(),
                                working_dir.clone(),
                                ToolExec {
                                    call,
                                    turn,
                                    round: step,
                                    authorization: crate::events::Authorization::No,
                                },
                            )
                            .await,
                        );
                    }
                    out
                }) as BoxFuture<'_, Vec<ToolResult>>
            })
            .await
    }
}

/// Drive one provider stream to completion, logging chunks as they arrive.
///
/// Chunks are logged verbatim, not re-rendered from the finished message: a
/// replay or a live UI has to be able to reproduce what actually streamed.
#[allow(clippy::too_many_arguments)]
async fn stream_once(
    provider: &dyn LlmProvider,
    ctx: &Context,
    session: &SessionLog,
    turn: u64,
    round: u32,
    messages: Vec<Message>,
    tools: Vec<ToolDef>,
    options: ChatOptions,
    idle: Option<std::time::Duration>,
) -> Result<ModelResponse, RequestError> {
    let mut stream = provider
        .chat_stream(&messages, &tools, &options)
        .await
        .map_err(|e| RequestError::from_provider(&e))?;
    let mut out = ModelResponse::default();
    loop {
        let next = match idle {
            Some(bound) => match tokio::time::timeout(bound, stream.next()).await {
                Ok(next) => next,
                // Silence is not an answer. Retryable, and carrying what already
                // streamed, so recovery can keep it.
                Err(_) => {
                    return Err(RequestError {
                        retryable: true,
                        code: Some(STREAM_IDLE_CODE.to_string()),
                        ..RequestError::message(format!(
                            "stream idle timeout: no event for {}ms",
                            bound.as_millis()
                        ))
                    }
                    .with_partial(out))
                }
            },
            None => stream.next().await,
        };
        let Some(event) = next else { break };
        match event {
            StreamEvent::TextDelta(text) => {
                // Through the one write path like every other fact. A chunk
                // that is appended and not broadcast is in memory and nowhere
                // else: persistence never stores it, so a resumed session has
                // no stream to replay.
                crate::session::commit(
                    ctx,
                    session,
                    SessionEvent::AssistantChunk {
                        turn,
                        round,
                        delta: text.clone(),
                        reasoning: false,
                    },
                );
                ctx.emit::<AssistantChunk>(&Chunk {
                    text: text.clone(),
                    reasoning: false,
                });
                out.text.push_str(&text);
            }
            StreamEvent::Reasoning(text) => {
                crate::session::commit(
                    ctx,
                    session,
                    SessionEvent::AssistantChunk {
                        turn,
                        round,
                        delta: text.clone(),
                        reasoning: true,
                    },
                );
                ctx.emit::<AssistantChunk>(&Chunk {
                    text: text.clone(),
                    reasoning: true,
                });
                out.reasoning.push_str(&text);
            }
            StreamEvent::ToolCall(call) => out.tool_calls.push(call),
            StreamEvent::Usage(usage) => out.usage = Some(usage),
            // Hand back what was already produced: the tokens were paid for,
            // and a retry that starts from nothing repeats work the model has
            // already done.
            StreamEvent::Error(err) => {
                return Err(RequestError::from_provider(&err).with_partial(out))
            }
            StreamEvent::Done { truncated } => out.truncated = truncated,
            _ => {}
        }
    }
    // A truncated response is not empty even when it looks it: the model was
    // cut off, which is a different situation with a different recovery.
    if out.truncated {
        return Ok(out);
    }

    // A response with neither text nor tool calls cannot advance the turn.
    // Reported as a typed failure so a retry policy can decide, rather than
    // silently producing an empty assistant message.
    if out.text.trim().is_empty() && out.tool_calls.is_empty() {
        return Err(RequestError::empty());
    }
    Ok(out)
}

#[async_trait]
impl AgentLoop for PluginAgentLoop {
    async fn drive(&self, agent: &Agent) -> TurnOutcome {
        // The whole turn runs as this agent, so a listener registered at the
        // top of the tree still resolves this agent's log and world.
        crate::agent::as_agent(agent.ctx().clone(), self.drive_as(agent)).await
    }
}

impl PluginAgentLoop {
    async fn drive_as(&self, agent: &Agent) -> TurnOutcome {
        // The agent's own context, so anything scoped to this agent — an
        // overridden tool catalog, an agent-local policy — is what resolves.
        let ctx = agent.ctx().clone();
        let session = agent.session();
        let Some(provider) = ctx.service::<LlmSvc>() else {
            return failed("no llm provider");
        };

        // Nothing waking in the inbox: no turn, no log entry, no cost. An
        // injection alone must never open a turn.
        if !agent.inbox().has_waking_input() {
            return TurnOutcome {
                stop: StopReason::Stopped,
                ..Default::default()
            };
        }

        // Opening the turn is what mints its cancellation token. Asking an
        // agent to stop must not outlive the turn it stopped.
        agent.begin_turn();
        let turn = session.next_turn();
        self.commit(&session, SessionEvent::TurnStart { turn });
        let mut outcome = TurnOutcome {
            turn,
            ..Default::default()
        };
        let started_at = std::time::Instant::now();
        let mut used_tokens = 0u32;
        let mut step: u32 = 0;

        loop {
            // Claim one message plus whatever context queued alongside it. On
            // later passes this is what folds a mid-turn message into the turn
            // already running instead of making it wait for the next one.
            let claimed = agent.inbox().claim();
            let mut decision = StepDecision {
                turn,
                step: step + 1,
                message: claimed.message,
                origin: claimed.origin,
                images: claimed.images,
                injections: claimed.injections,
                rejected: None,
                starts_request_series: step == 0,
            };

            if !decision.is_empty() || step == 0 {
                decision = self
                    .ctx
                    .waterfall::<PreStep, _>(&mut decision, |d| {
                        let d = d.clone();
                        Box::pin(async move { d })
                    })
                    .await;

                if let Some(reason) = decision.rejected.clone() {
                    // The attempt is a fact even though nothing was sent: a
                    // turn that was refused must be visible in the log.
                    outcome.stop = StopReason::InputRejected;
                    outcome.error = Some(reason);
                    break;
                }
                if decision.is_empty() && step == 0 {
                    // A first claim rewritten to nothing closes the turn rather
                    // than sending a request with no new input.
                    outcome.stop = StopReason::Stopped;
                    break;
                }
            }

            // Announced once the first input is known, and before anything is
            // committed: a listener that injects context on turn start (memory,
            // a reminder) must land ahead of the message it accompanies.
            if step == 0 {
                self.ctx.emit::<TurnStart>(&TurnStarted {
                    prompt: decision.message.clone().unwrap_or_default(),
                    turn,
                });
                // Whatever that listener queued is claimed now rather than next
                // step, so first-turn context reaches the first request.
                let late = agent.inbox().claim_injections();
                for (text, origin) in late {
                    decision.injections.push((text, origin));
                }
            }

            // Everything the model will see enters the log before the request
            // is assembled — never alongside it.
            for (text, origin) in &decision.injections {
                self.commit(
                    &session,
                    SessionEvent::Injected {
                        turn,
                        text: text.clone(),
                        origin: origin.clone(),
                    },
                );
            }
            // A round answering a nudge the harness wrote may legitimately have
            // nothing to add: the model read the note, did not need a tool, and
            // has already answered the person. An ordinary round with no content
            // is a provider that failed; this one is a turn that is finished.
            let answering_a_nudge =
                matches!(decision.origin, MessageOrigin::Internal) && decision.message.is_some();
            if let Some(text) = &decision.message {
                // The same wake, two provenances. A continuation the harness
                // scheduled is model-visible like anything else and is logged
                // as the harness's, so a replayed transcript never attributes
                // it to the user.
                let event = match decision.origin {
                    MessageOrigin::User => SessionEvent::UserMessage {
                        turn,
                        text: text.clone(),
                        images: decision.images.clone(),
                    },
                    MessageOrigin::Harness => SessionEvent::Injected {
                        turn,
                        text: text.clone(),
                        origin: InjectionOrigin::Continuation,
                    },
                    MessageOrigin::Internal => SessionEvent::Injected {
                        turn,
                        text: text.clone(),
                        origin: InjectionOrigin::InternalNudge,
                    },
                    // Logged under the sender's session id, not its registry
                    // id: the log outlives both agents.
                    MessageOrigin::Peer(sender) => SessionEvent::Injected {
                        turn,
                        text: text.clone(),
                        origin: InjectionOrigin::Peer {
                            from: self
                                .ctx
                                .service::<crate::seams::AgentsSvc>()
                                .and_then(|a| a.get(sender))
                                .map(|a| a.session_id().to_string())
                                .unwrap_or_else(|| format!("agent-{sender}")),
                        },
                    },
                };
                self.commit(&session, event);
            }

            step += 1;
            outcome.steps = step;
            outcome.rounds = step;
            self.commit(&session, SessionEvent::StepStart { turn, step });

            let (messages, tools) = self.assemble(&session, &ctx);

            if self.verify_log_invariant {
                if let Err(unexplained) =
                    crate::session::assert_model_visible_is_logged(&session, &messages)
                {
                    outcome.stop = StopReason::InvariantViolated;
                    outcome.error = Some(format!(
                        "model-visible content is not in the session log: {}",
                        unexplained.join("; ")
                    ));
                    break;
                }
            }

            self.commit(
                &session,
                SessionEvent::RequestHeader {
                    turn,
                    round: step,
                    model: provider.model_name().to_string(),
                    reason: if decision.starts_request_series {
                        HeaderReason::Series
                    } else {
                        HeaderReason::Append
                    },
                },
            );

            // Race the request against the stop button.
            //
            // Checking `cancelled()` between rounds is not enough: a model
            // answer takes seconds, and a cancel that only lands after it does
            // means a person who pressed stop waits for the whole reply. The
            // events are identical either way — they just arrive late — so this
            // is invisible to an event-stream comparison and was caught by
            // timing a cancel against `atomcode-coding`, which aborts promptly.
            let asked = self.request(
                provider.clone(),
                session.clone(),
                &ctx,
                turn,
                step,
                ModelRequest {
                    messages,
                    tools,
                    options: ChatOptions::default(),
                    turn,
                    round: step,
                    answering_a_nudge,
                },
            );
            let cancel = agent.cancel_token();
            let request_started = std::time::Instant::now();
            let response = match tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    outcome.stop = StopReason::Cancelled;
                    break;
                }
                answered = asked => answered,
            } {
                Ok(response) => response,
                Err(error) if error.empty_response && answering_a_nudge => {
                    // Nothing to add to a note nobody asked for: the turn ends
                    // where it already was.
                    outcome.stop = StopReason::Stopped;
                    break;
                }
                Err(error) => {
                    if let Some(pause) = error.rate_limit_pause.clone() {
                        // Not a failure: the turn stops cleanly and says when to
                        // come back.
                        self.commit(&session, SessionEvent::RateLimitPaused { turn, pause });
                        outcome.stop = StopReason::RateLimited;
                        break;
                    }
                    outcome.stop = if error.code.as_deref() == Some(STREAM_IDLE_CODE) {
                        StopReason::Timeout
                    } else {
                        StopReason::ProviderError
                    };
                    outcome.error = Some(error.to_string());
                    break;
                }
            };

            if let Some(usage) = response.usage {
                used_tokens = usage.prompt;
                self.commit(
                    &session,
                    SessionEvent::Usage {
                        turn,
                        round: step,
                        usage,
                    },
                );
            }
            let meta = response_meta(
                &session,
                provider.as_ref(),
                &response,
                turn,
                step,
                request_started.elapsed(),
            );
            self.commit(
                &session,
                SessionEvent::AssistantMessage {
                    turn,
                    round: step,
                    text: response.text.clone(),
                    reasoning: response.reasoning.clone(),
                    tool_calls: response.tool_calls.clone(),
                    reasoning_blocks: Vec::new(),
                    meta: Some(meta),
                },
            );
            self.ctx.emit::<AssistantMessage>(&Message::assistant(
                response.text.clone(),
                response.tool_calls.clone(),
            ));
            if !response.text.is_empty() {
                outcome.text = response.text.clone();
            }

            let tool_count = response.tool_calls.len() as u32;
            outcome.tool_calls += tool_count;
            // No calls, nothing to schedule: dispatching an empty batch would
            // make every listener reason about a round that did not happen.
            let results = if tool_count == 0 {
                Vec::new()
            } else {
                self.execute_batch(&ctx, agent, response.tool_calls, turn, step)
                    .await
            };
            // Side effects were already applied concurrently; the log is written
            // in emission order so the transcript matches what the model asked
            // for, not what happened to finish first.
            for result in results {
                self.commit(
                    &session,
                    SessionEvent::ToolResultLogged {
                        turn,
                        round: step,
                        call_id: result.call_id.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                        images: result.images.clone(),
                    },
                );
                self.ctx.emit::<ToolResultEvent>(&result);
            }
            self.commit(
                &session,
                SessionEvent::StepEnd {
                    turn,
                    step,
                    tool_calls: tool_count,
                },
            );

            // Give the runtime a turn of its own before deciding to continue.
            // Cancellation is cooperative, which means someone has to be able
            // to *deliver* it: a round that resolves entirely from memory — a
            // cached response, a replayed fixture, a tool that hits nothing —
            // never yields, so on a single-threaded runtime the task holding
            // the stop button is never scheduled and the turn runs to the end
            // regardless. One yield per round makes the guarantee the loop
            // already claims independent of what the provider happens to do.
            tokio::task::yield_now().await;
            if agent.cancelled() {
                outcome.stop = StopReason::Cancelled;
                break;
            }

            let owes_a_request = tool_count > 0 || response.truncated;
            if let Some(stop) = self
                .ctx
                .serial::<TurnStopping>(&TurnProgress {
                    turn,
                    rounds: step,
                    tool_calls: outcome.tool_calls,
                    used_tokens,
                    elapsed: started_at.elapsed(),
                    continuing: owes_a_request || agent.inbox().has_waking_input(),
                })
                .await
            {
                outcome.stop = stop;
                break;
            }

            // The turn continues while anything is owed: tools produced results
            // the model has not seen, someone put more work in the inbox, or
            // the answer was cut off at the output limit and the recovery row
            // has asked the model to resume.
            //
            // Truncation used to end the turn here, which made the
            // `truncation-recovery` row half a feature: it injected the coaching
            // text and nothing ever asked for the rest. The person got half a
            // sentence. That row still owns the *policy* — when it stops
            // nudging it clears the flag, and this condition goes false — so the
            // continuation cap stays in one place.
            if !owes_a_request && !agent.inbox().has_waking_input() {
                outcome.stop = StopReason::Stopped;
                break;
            }

            if step >= self.max_rounds {
                outcome.stop = StopReason::RunawayFuse;
                break;
            }
        }

        if outcome.stop == StopReason::Cancelled && agent.take_interrupted() {
            self.commit(
                &session,
                SessionEvent::Interrupted {
                    turn,
                    undone: self.undo_cancelled,
                },
            );
        }
        self.ctx.parallel::<TurnFinishing>(&outcome).await;
        self.commit(
            &session,
            SessionEvent::TurnEnd {
                turn,
                stop: outcome.stop,
                error: outcome.error.clone(),
            },
        );
        self.ctx.emit::<TurnEnd>(&outcome);
        // Idle under a fresh token even after a cancel: the turn that was
        // stopping has stopped, and an agent left cancelled between turns is
        // one every later turn ends before its first request.
        agent.end_turn();
        outcome
    }
}

/// The stats a stored message carries about the response that produced it.
///
/// `request_id` continues the session's own sequence — one past the highest id
/// any logged response already holds — rather than restarting per process, so a
/// session resumed from a store that kept those ids does not mint duplicates.
fn response_meta(
    session: &SessionLog,
    provider: &dyn LlmProvider,
    response: &ModelResponse,
    turn: u64,
    round: u32,
    elapsed: std::time::Duration,
) -> atomcode_kernel::message::MessageMeta {
    let request_id = session
        .events()
        .iter()
        .filter_map(|logged| match &logged.event {
            SessionEvent::AssistantMessage {
                meta: Some(meta), ..
            } => Some(meta.request_id),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + 1;
    let tokens = response.usage.unwrap_or_default();
    let ctx_window = provider.context_window();
    let used_tokens = tokens.prompt;
    atomcode_kernel::message::MessageMeta {
        tokens,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        ctx_window,
        used_tokens,
        utilization: if ctx_window == 0 {
            0.0
        } else {
            used_tokens as f32 / ctx_window as f32
        },
        round,
        turn_id: turn,
        request_id,
        session_id: Some(session.id().to_string()),
        finish_reason: if !response.tool_calls.is_empty() {
            "tool_calls"
        } else if response.truncated {
            "length"
        } else {
            "stop"
        }
        .to_string(),
        ..Default::default()
    }
}

fn failed(message: &str) -> TurnOutcome {
    TurnOutcome {
        stop: StopReason::ProviderError,
        error: Some(message.to_string()),
        ..Default::default()
    }
}

/// Append this request's shape to `$ATOMCODE_DUMP_REQUEST`, one JSON line per
/// call. Off unless that variable is set, so nothing changes for anyone else.
///
/// It exists to answer one question the log cannot: **what did the resumed
/// process actually send?** The request is `system prompt + derive_messages`,
/// and both halves are visible here — the second is a pure function of the log,
/// so a resumed request that differs from the live one can only differ because
/// the *log the process holds* differs. The `events`/`max_seq` fields say which
/// log that was; the per-message hashes say what came out of it. Comparing two
/// dumps finds the first message that changed.
///
/// Deliberately a side channel rather than a log fact: it is a debugging
/// instrument, it must not enter the session, and a `build_request_body` that
/// grew a diagnostic would be a wire change.
fn dump_request(session: &SessionLog, messages: &[Message], tools: &[ToolDef]) {
    // The name and the resolution live in `model_source`; this only consumes.
    let Some(path) = crate::model_source::request_dump_path() else {
        return;
    };
    use std::hash::{Hash, Hasher};
    let digest = |s: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.hash(&mut h);
        format!("{:016x}", h.finish())
    };
    let events = session.events();
    let max_seq = events.iter().map(|e| e.seq).max().unwrap_or(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let mut out = format!(
        "{{\"ts\":{ms},\"session\":{:?},\"events\":{},\"max_seq\":{},\"n_msgs\":{},\"total_chars\":{},\
         \"tool_defs\":{},\"msgs\":[",
        session.id(),
        events.len(),
        max_seq,
        messages.len(),
        messages.iter().map(|m| m.text.len()).sum::<usize>(),
        tools.len(),
    );
    for (i, m) in messages.iter().enumerate() {
        let r = m.reasoning.as_deref().unwrap_or("");
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"i\":{i},\"role\":{:?},\"len\":{},\"h\":\"{}\",\"rlen\":{},\"rh\":\"{}\",\"calls\":{}}}",
            format!("{:?}", m.role),
            m.text.len(),
            digest(&m.text),
            r.len(),
            digest(r),
            m.tool_calls.len(),
        ));
    }
    out.push_str("]}\n");

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = f.write_all(out.as_bytes());
    }
}

pub struct AgentLoopPlugin;

#[async_trait]
impl Plugin for AgentLoopPlugin {
    fn name(&self) -> &'static str {
        "agent-loop"
    }
    fn inject(&self) -> &'static [&'static str] {
        // `system-prompt` and `session-projections` are deliberately absent: a
        // harness without them still runs, it just sends no system message and
        // folds no state. Injecting them would make optional contributors hard
        // dependencies.
        &["llm", "tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Resolved per round, and the turn runs without them: no system message
        // when there is no registry, no folded state when nothing projects, no
        // progress line or question for a tool when no front end drives it.
        &["system-prompt", "session-projections", "tool-driver"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["agent-loop"]
    }
    fn description(&self) -> &'static str {
        "the default turn driver: assemble from the log, request, execute tools, repeat"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: LoopRow = if config.is_null() {
            LoopRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let working_dir = row
            .working_dir
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let driver = PluginAgentLoop {
            ctx: ctx.clone(),
            working_dir,
            max_rounds: row.max_rounds,
            verify_log_invariant: row.verify_log_invariant,
            undo_cancelled: row.undo_cancelled,
            stream_idle: (row.stream_idle_ms > 0)
                .then(|| std::time::Duration::from_millis(row.stream_idle_ms)),
        };
        let _ = ctx
            .provide::<AgentLoopSvc>(Arc::new(driver))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- driving an agent nobody holds a handle on -----------------------------

/// An agent kept running by its inbox: whenever a message lands and no turn is
/// in flight, one starts. Dropping this stops listening and stops the task.
///
/// For agents that are not behind a driver protocol — a team member, a goal
/// the harness runs on its own. The handle pump does the same for the agent a
/// driver holds.
pub struct Driving {
    _wake: atomcode_plexus::Disposable,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Driving {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn keep_driven(agent: Arc<Agent>) -> Result<Driving, String> {
    let driver = agent
        .ctx()
        .require::<AgentLoopSvc>()
        .map_err(|e| e.to_string())?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let me = agent.id();
    let wake = agent
        .ctx()
        .on_emit::<InboxInserted>(move |info: &AgentInfo| {
            if info.id == me {
                let _ = tx.send(());
            }
        });
    let task = tokio::spawn(async move {
        // Whatever was queued before anyone listened counts as a wake.
        while agent.inbox().has_waking_input() {
            driver.drive(&agent).await;
        }
        // Wakes are edge-triggered and the queue is level-checked: several
        // arriving mid-turn collapse into one look at the inbox afterwards.
        while rx.recv().await.is_some() {
            while agent.inbox().has_waking_input() {
                driver.drive(&agent).await;
            }
        }
    });
    Ok(Driving { _wake: wake, task })
}
