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

use crate::agent::{Agent, AgentStatus};
use crate::events::{
    AgentRequest, AssistantChunk, AssistantMessage, Chunk, ModelRequest, ModelResponse, PreStep,
    RequestError, SessionEventCommitted, StepDecision, ToolBatch, ToolExec, ToolResultEvent,
    ToolsExecuteBatch, TurnEnd, TurnProgress, TurnStart, TurnStarted, TurnStopping,
};
use crate::seams::{
    AgentLoop, AgentLoopSvc, LlmSvc, SessionProjectionsSvc, SessionSvc, StopReason,
    SystemPromptSvc, ToolsSvc, TurnOutcome,
};
use crate::session::{HeaderReason, LoggedEvent, SeqNo, SessionEvent, SessionLog};

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
}

impl Default for LoopRow {
    fn default() -> Self {
        Self {
            max_rounds: default_max_rounds(),
            working_dir: None,
            verify_log_invariant: true,
        }
    }
}

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
}

impl PluginAgentLoop {
    /// Commit one fact: append, broadcast, advance projections.
    ///
    /// The single write path. A plugin that wants to react to session state
    /// listens to `session/event`; it never has to be called by the loop.
    fn commit(&self, session: &SessionLog, event: SessionEvent) -> SeqNo {
        let seq = session.append(event.clone());
        self.ctx
            .emit::<SessionEventCommitted>(&LoggedEvent { seq, event });
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
                Box::pin(async move {
                    stream_once(
                        &*provider, &ctx, &session, turn, round, messages, tools, options,
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
            working_dir: self.working_dir.clone(),
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
                                    pre_approved: false,
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
) -> Result<ModelResponse, RequestError> {
    let mut stream = provider
        .chat_stream(&messages, &tools, &options)
        .await
        .map_err(|e| RequestError::from_provider(&e))?;
    let mut out = ModelResponse::default();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta(text) => {
                session.append(SessionEvent::AssistantChunk {
                    turn,
                    round,
                    delta: text.clone(),
                    reasoning: false,
                });
                ctx.emit::<AssistantChunk>(&Chunk {
                    text: text.clone(),
                    reasoning: false,
                });
                out.text.push_str(&text);
            }
            StreamEvent::Reasoning(text) => {
                session.append(SessionEvent::AssistantChunk {
                    turn,
                    round,
                    delta: text.clone(),
                    reasoning: true,
                });
                ctx.emit::<AssistantChunk>(&Chunk {
                    text: text.clone(),
                    reasoning: true,
                });
                out.reasoning.push_str(&text);
            }
            StreamEvent::ToolCall(call) => out.tool_calls.push(call),
            StreamEvent::Usage(usage) => out.usage = Some(usage),
            StreamEvent::Error(err) => return Err(RequestError::from_provider(&err)),
            _ => {}
        }
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
        // The agent's own context, so anything scoped to this agent — an
        // overridden tool catalog, an agent-local policy — is what resolves.
        let ctx = agent.ctx().clone();
        let Some(session) = ctx.service::<SessionSvc>() else {
            return failed("no session log");
        };
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

        agent.set_status(AgentStatus::Working);
        let turn = session.open_turn();
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
                        origin: *origin,
                    },
                );
            }
            if let Some(text) = &decision.message {
                self.commit(
                    &session,
                    SessionEvent::UserMessage {
                        turn,
                        text: text.clone(),
                        images: Vec::new(),
                    },
                );
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

            let response = match self
                .request(
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
                    },
                )
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    outcome.stop = StopReason::ProviderError;
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
            self.commit(
                &session,
                SessionEvent::AssistantMessage {
                    turn,
                    round: step,
                    text: response.text.clone(),
                    reasoning: response.reasoning.clone(),
                    tool_calls: response.tool_calls.clone(),
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
            let results = self
                .execute_batch(&ctx, agent, response.tool_calls, turn, step)
                .await;
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

            if agent.cancelled() {
                outcome.stop = StopReason::Cancelled;
                break;
            }

            if let Some(stop) = self
                .ctx
                .serial::<TurnStopping>(&TurnProgress {
                    turn,
                    rounds: step,
                    tool_calls: outcome.tool_calls,
                    used_tokens,
                    elapsed: started_at.elapsed(),
                })
                .await
            {
                outcome.stop = stop;
                break;
            }

            // The turn continues while anything is owed: tools produced results
            // the model has not seen, or someone put more work in the inbox.
            let owes_a_request = tool_count > 0;
            if !owes_a_request && !agent.inbox().has_waking_input() {
                outcome.stop = StopReason::Stopped;
                break;
            }

            if step >= self.max_rounds {
                outcome.stop = StopReason::RunawayFuse;
                break;
            }
        }

        self.commit(
            &session,
            SessionEvent::TurnEnd {
                turn,
                stop: format!("{:?}", outcome.stop),
                error: outcome.error.clone(),
            },
        );
        self.ctx.emit::<TurnEnd>(&outcome);
        agent.set_status(if agent.cancelled() {
            AgentStatus::Stopping
        } else {
            AgentStatus::Idle
        });
        outcome
    }
}

fn failed(message: &str) -> TurnOutcome {
    TurnOutcome {
        stop: StopReason::ProviderError,
        error: Some(message.to_string()),
        ..Default::default()
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
        &["llm", "tools", "sessions"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Resolved per round, and the turn runs without them: no system message
        // when there is no registry, no folded state when nothing projects.
        &["system-prompt", "session-projections"]
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
        };
        let _ = ctx
            .provide::<AgentLoopSvc>(Arc::new(driver))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
