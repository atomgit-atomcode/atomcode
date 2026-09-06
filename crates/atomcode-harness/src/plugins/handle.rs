//! The driver-protocol front end: this harness, behind the handle AtomCode's
//! shipped UIs already speak.
//!
//! `atomcode-tuix`, the daemon and the WebUI all drive an agent through one
//! channel pair — [`AgentCommand`] in, [`AgentEvent`] out. That pair is a
//! *protocol*, not an implementation: nothing in it says who runs the turn. So
//! it is a front end like any other, filling the `ui` slot, differing only in
//! that the thing on the other end is a program which already knows how to
//! render a conversation.
//!
//! # It is a projection, not a wrapper
//!
//! The harness stays the source of truth. It runs its own loop, writes its own
//! log, and decides approvals through its own seams; this row reads those facts
//! and says them in another vocabulary. The two directions share nothing:
//!
//! ```text
//!   session log ──(fold)──> AgentEvent  ──> the driver
//!   the driver  ──────────> AgentCommand ──> inbox / cancel / compaction
//! ```
//!
//! Which is why the outward half is a fold over committed events rather than a
//! set of calls sprinkled through the loop: a replayed session and a live one
//! produce the same screen, because they are the same events.
//!
//! # One conversation
//!
//! Every listener above a delegating agent sees the child's facts too — that is
//! what one-way realm visibility means. A driver rendering one conversation
//! must not receive two, so the fold is filtered to the log this handle speaks
//! for. The subagent's transcript stays where it belongs: in the subagent.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use atomcode_capabilities::tools::approval::{ApprovalRequest, PermissionDecision, APPROVAL_KIND};
use atomcode_capabilities::tools::request_user_input::{
    UserInputMode, UserInputOption, UserInputRequest, UserInputResponse, REQUEST_USER_INPUT_KIND,
};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand, AgentEvent, RequestId, ToolBatchCall};
use atomcode_kernel::message::{CompactTrigger, MessageMeta, SessionSnapshot};
use atomcode_kernel::tool::{RiskLevel, Tool};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::agent::{Agent, MessageOrigin};
use crate::events::{AgentCreated, AgentInfo, SessionEventCommitted};
use crate::seams::{
    AgentHandleSource, AgentHandleSvc, AgentLoopSvc, AgentsSvc, ApprovalPolicy, CompactionSvc,
    Decision, LlmSvc, SessionSvc, ToolBox, ToolsSvc, UiSvc, UserInterface, UserQuestions,
    UserQuestionsSvc,
};
use crate::session::{Committed, SessionEvent, SessionLog};

// ---- outward: session facts, as a driver's events -----------------------

/// A tool batch the last assistant message opened.
struct OpenBatch {
    id: String,
    total: usize,
    ok: usize,
    started: Instant,
}

/// The fold that turns committed facts into driver events.
///
/// Stateful, because two of the driver's events describe a *span* rather than a
/// moment: a batch opens when an assistant message asks for several calls and
/// closes when the step that ran them ends. Everything else is a straight
/// rename.
struct Projector {
    /// Resolved live, so a tool mounted mid-session is classified correctly.
    tools: Option<Arc<ToolBox>>,
    ctx_window: u32,
    batch: Option<OpenBatch>,
    last_prompt_tokens: u32,
    /// How many user messages this turn has taken. The second and later ones
    /// are steering.
    said_this_turn: u32,
}

impl Projector {
    /// Whether this tool is one the catalog will actually dispatch.
    ///
    /// Announces when it cannot tell. Suppressing on "no catalog visible" would
    /// drop *every* `ToolStarted` in a tree with no `tools` row — silently, and
    /// only for the drivers that need those events most. The rule is: stay
    /// quiet only when we know for certain the tool is absent.
    fn mounted(&self, name: &str) -> bool {
        match self.tools.as_ref() {
            Some(tools) => tools.get(name).is_some(),
            None => true,
        }
    }

    fn parallel_safe(&self, name: &str, arguments: &str) -> bool {
        self.tools
            .as_ref()
            .and_then(|t| t.get(name))
            .map(|t: Arc<dyn Tool>| t.parallel_safe(arguments))
            // Unknown tool: it is about to fail anyway, and claiming a call the
            // catalog does not have is parallel-safe would be a guess about
            // side effects nobody can back up.
            .unwrap_or(false)
    }

    fn project(&mut self, event: &SessionEvent) -> Vec<AgentEvent> {
        match event {
            SessionEvent::TurnStart { .. } => {
                self.said_this_turn = 0;
                vec![AgentEvent::TurnStarted]
            }

            SessionEvent::AssistantChunk {
                delta, reasoning, ..
            } => {
                if *reasoning {
                    vec![AgentEvent::Reasoning(delta.clone())]
                } else {
                    vec![AgentEvent::TextDelta(delta.clone())]
                }
            }

            // The assistant message is committed before its tools run, so the
            // started events land ahead of every result — the ordering a
            // grouped UI block depends on.
            SessionEvent::AssistantMessage {
                turn,
                round,
                tool_calls,
                ..
            } => {
                let mut out = Vec::new();
                if tool_calls.len() >= 2 {
                    let id = format!("batch-{turn}-{round}");
                    out.push(AgentEvent::ToolBatchStarted {
                        batch_id: id.clone(),
                        calls: tool_calls
                            .iter()
                            .map(|c| ToolBatchCall {
                                id: c.id.clone(),
                                name: c.name.clone(),
                                arguments: c.arguments.clone(),
                                parallel_safe: self.parallel_safe(&c.name, &c.arguments),
                            })
                            .collect(),
                    });
                    self.batch = Some(OpenBatch {
                        id,
                        total: tool_calls.len(),
                        ok: 0,
                        started: Instant::now(),
                    });
                }
                for call in tool_calls {
                    // Only for a tool that will actually run. A model naming a
                    // tool nobody mounted is ordinary, and announcing that it
                    // *started* is a lie the driver then has to unpick — the
                    // reference engine goes straight to the failed result, and
                    // a differential run showed this as the only difference on
                    // that path.
                    if self.mounted(&call.name) {
                        out.push(AgentEvent::ToolStarted { call: call.clone() });
                    }
                }
                out
            }

            SessionEvent::ToolResultLogged {
                call_id,
                content,
                is_error,
                images,
                ..
            } => {
                if let Some(batch) = self.batch.as_mut() {
                    if !*is_error {
                        batch.ok += 1;
                    }
                }
                vec![AgentEvent::ToolResult {
                    result: atomcode_kernel::tool::ToolResult {
                        call_id: call_id.clone(),
                        content: content.clone(),
                        is_error: *is_error,
                        images: images.clone(),
                    },
                }]
            }

            SessionEvent::StepEnd { .. } => match self.batch.take() {
                Some(batch) => vec![AgentEvent::ToolBatchCompleted {
                    batch_id: batch.id,
                    ok: batch.ok,
                    total: batch.total,
                    elapsed_ms: batch.started.elapsed().as_millis() as u64,
                }],
                None => Vec::new(),
            },

            SessionEvent::Usage { turn, round, usage } => {
                self.last_prompt_tokens = usage.prompt;
                vec![AgentEvent::Usage(MessageMeta {
                    tokens: *usage,
                    ctx_window: self.ctx_window,
                    used_tokens: usage.prompt,
                    utilization: if self.ctx_window == 0 {
                        0.0
                    } else {
                        usage.prompt as f32 / self.ctx_window as f32
                    },
                    round: *round,
                    turn_id: *turn,
                    request_id: *round as u64,
                    ..Default::default()
                })]
            }

            SessionEvent::Compacted { summary, .. } => vec![AgentEvent::Compacted {
                trigger: CompactTrigger::Auto {
                    utilization: if self.ctx_window == 0 {
                        0.0
                    } else {
                        self.last_prompt_tokens as f32 / self.ctx_window as f32
                    },
                },
                epoch: 0,
                removed: 0,
                bytes_before: 0,
                bytes_after: summary.len(),
                committed: true,
                snapshot: None,
            }],

            // Advisory: the turn continues. A driver renders it as a note, not
            // as a failure — a rate-limit wait is not an error.
            // Typed, not flattened. The protocol has a dedicated event for each
            // of these and a driver renders them differently — a rate limit
            // wants a countdown, a retry wants an attempt counter. Sending them
            // all as `Warning(String)` hands the driver prose to parse, and
            // parsing prose is how a UI ends up wrong in a language nobody
            // tested.
            SessionEvent::Notice { notice, detail, .. } => {
                vec![match notice {
                    crate::session::NoticeKind::RateLimited => AgentEvent::RateLimited {
                        reset_at_display: detail.clone(),
                        reset_label: detail.clone(),
                        secs_until_reset: None,
                        auto_resuming: true,
                        server_message: Some(detail.clone()),
                    },
                    crate::session::NoticeKind::ProviderRetry => AgentEvent::ProviderRetry {
                        attempt: 0,
                        max_attempts: 0,
                        backoff_secs: 0,
                        reason: detail.clone(),
                    },
                    crate::session::NoticeKind::StreamRecovered => AgentEvent::StreamRecovery {
                        attempt: 0,
                        max_attempts: 0,
                        recovered: true,
                    },
                    crate::session::NoticeKind::OutputTruncated => {
                        AgentEvent::OutputTruncationRecovery {
                            attempt: 0,
                            max_attempts: 0,
                        }
                    }
                    // Compaction is not a recovery the protocol names; it stays
                    // a warning rather than being forced into a shape that means
                    // something else.
                    crate::session::NoticeKind::OverflowCompacted => {
                        AgentEvent::Warning(detail.clone())
                    }
                }]
            }

            SessionEvent::TurnEnd { stop, error, .. } => {
                let mut out = Vec::new();
                if let Some(message) = error {
                    out.push(AgentEvent::Error {
                        message: message.clone(),
                        http_status: None,
                        code: None,
                        retryable: None,
                    });
                }
                if matches!(stop, crate::seams::StopReason::Cancelled) {
                    out.push(AgentEvent::Cancelled);
                }
                out.push(AgentEvent::TurnComplete {
                    reason: stop_reason(*stop),
                });
                out
            }

            // Deliberately silent. A user message is what the driver just sent,
            // an injection is context the kernel protocol also hides from
            // user-facing projections, and step/request boundaries are the
            // harness's own bookkeeping.
            //
            // Listed rather than swept up by a wildcard: a new fact should not
            // be able to become invisible to every driver by default. Adding
            // one stops compiling here until someone decides what it looks
            // like on a screen.
            // A second user message inside one turn is steering: the person
            // typed while the model was answering and the loop folded it in.
            // The behaviour was already right — one turn, not two — but the
            // driver was never told, so a UI could not say "your message was
            // folded into this turn". The reference engine announces it; a
            // differential run showed this as the only difference on that path.
            SessionEvent::UserMessage { text, images, .. } => {
                if self.said_this_turn == 0 {
                    self.said_this_turn = 1;
                    Vec::new()
                } else {
                    self.said_this_turn += 1;
                    vec![AgentEvent::Steered {
                        count: 1,
                        inputs: vec![atomcode_kernel::event::SteeredInput {
                            text: text.clone(),
                            images: images.clone(),
                        }],
                    }]
                }
            }
            SessionEvent::Injected { .. }
            | SessionEvent::StepStart { .. }
            | SessionEvent::RequestHeader { .. } => Vec::new(),
        }
    }
}

/// This harness's stop reasons, in the driver's vocabulary.
///
/// Where the two disagree the mapping is deliberate rather than clever:
/// `StoppedByPolicy` and `RunawayFuse` are both budgets running out, and
/// `InvariantViolated` has no counterpart at all — it rides out as a failure
/// with the real cause in the `Error` that precedes it, because the one thing a
/// driver must never do is read it as a clean stop.
fn stop_reason(stop: crate::seams::StopReason) -> atomcode_kernel::event::StopReason {
    use crate::seams::StopReason as In;
    use atomcode_kernel::event::StopReason as Out;
    match stop {
        In::Stopped => Out::Stopped,
        In::MaxRounds | In::StoppedByPolicy | In::RunawayFuse => Out::MaxRounds,
        In::ProviderError | In::InvariantViolated => Out::ProviderError,
        In::ToolLoopDetected => Out::ToolLoopDetected,
        In::Cancelled => Out::Cancelled,
        In::InputRejected => Out::PromptRejected,
    }
}

// ---- inward: asking the driver ------------------------------------------

/// The half that asks. Fills both the `approval` and `user-questions` seams,
/// because a driver that can render a prompt can answer either.
struct Asker {
    /// Dropped on shutdown. The asker outlives the pump — it is a service in
    /// the tree — so a sender it held forever would keep the event channel
    /// open forever, and a driver reading to the end would never reach one.
    events: Mutex<Option<mpsc::UnboundedSender<AgentEvent>>>,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    /// Calls the driver said to stop asking about, as `(tool, arguments)`.
    /// Exact bytes, not the tool name: "always allow this" is a statement about
    /// the call that was shown, not about everything the tool can do.
    granted: Mutex<HashSet<(String, String)>>,
    timeout: Duration,
}

impl Asker {
    fn new(events: mpsc::UnboundedSender<AgentEvent>, timeout: Duration) -> Self {
        Self {
            events: Mutex::new(Some(events)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            granted: Mutex::new(HashSet::new()),
            timeout,
        }
    }

    /// Deliver an answer. `false` means nobody was waiting for it.
    fn answer(&self, id: RequestId, value: Value) -> bool {
        let waiting = self.pending.lock().expect("pending poisoned").remove(&id);
        match waiting {
            Some(tx) => tx.send(value).is_ok(),
            None => false,
        }
    }

    /// Shut the asking half down: no more questions, and every one still
    /// waiting is refused. A caller blocked on an answer that is never coming
    /// would hold the turn open forever.
    fn close(&self) {
        self.events.lock().expect("events poisoned").take();
        self.refuse_all();
    }

    /// Every pending question, refused at once.
    fn refuse_all(&self) {
        let waiting: Vec<_> = self
            .pending
            .lock()
            .expect("pending poisoned")
            .drain()
            .collect();
        for (_, tx) in waiting {
            let _ = tx.send(Value::Null);
        }
    }

    /// One round-trip. `None` means no answer arrived, which every caller must
    /// read as a refusal — never as consent.
    async fn request(&self, kind: &str, payload: Value) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending poisoned")
            .insert(id, tx);
        let sent = match self.events.lock().expect("events poisoned").as_ref() {
            Some(events) => events.send(AgentEvent::Request {
                id,
                kind: kind.to_string(),
                payload,
            }),
            None => Err(mpsc::error::SendError(AgentEvent::TurnStarted)),
        };
        // Nobody on the other end: a refusal, not a wait.
        if sent.is_err() {
            self.pending.lock().expect("pending poisoned").remove(&id);
            return None;
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(value)) => Some(value),
            _ => {
                self.pending.lock().expect("pending poisoned").remove(&id);
                None
            }
        }
    }
}

#[async_trait]
impl ApprovalPolicy for Asker {
    async fn decide(
        &self,
        call: &atomcode_kernel::tool::ToolCall,
        tool: &Arc<dyn Tool>,
    ) -> Decision {
        if matches!(tool.risk(&call.arguments), RiskLevel::Safe) {
            return Decision::Allow;
        }
        let key = (call.name.clone(), call.arguments.clone());
        if self.granted.lock().expect("grants poisoned").contains(&key) {
            return Decision::Allow;
        }
        let request = ApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            // The exact bytes that will execute. Approving a rendering of a
            // call and running a different one is the whole failure this
            // contract exists to prevent.
            args: call.arguments.clone(),
        };
        let answer = self
            .request(
                APPROVAL_KIND,
                serde_json::to_value(&request).unwrap_or(Value::Null),
            )
            .await;
        // A missing answer parses as deny, which is the point: `from_value`
        // fails closed on `Null`.
        match PermissionDecision::from_value(&answer.unwrap_or(Value::Null)) {
            PermissionDecision::AllowOnce => Decision::Allow,
            PermissionDecision::AllowAlways => {
                self.granted.lock().expect("grants poisoned").insert(key);
                Decision::Allow
            }
            PermissionDecision::Deny => Decision::Deny(format!("`{}` was not approved", call.name)),
        }
    }
}

#[async_trait]
impl UserQuestions for Asker {
    fn describe(&self) -> String {
        "the connected driver".into()
    }

    async fn ask(&self, question: &str, options: &[String]) -> Option<String> {
        let request = UserInputRequest {
            header: "Question".into(),
            question: question.to_string(),
            mode: if options.is_empty() {
                UserInputMode::Text
            } else {
                UserInputMode::Single
            },
            options: options
                .iter()
                .map(|label| UserInputOption {
                    label: label.clone(),
                    description: None,
                })
                .collect(),
            custom: true,
        };
        let answer = self
            .request(
                REQUEST_USER_INPUT_KIND,
                serde_json::to_value(&request).unwrap_or(Value::Null),
            )
            .await?;
        let response: UserInputResponse = serde_json::from_value(answer).ok()?;
        if response.declined {
            return None;
        }
        response
            .text
            .or_else(|| response.selected.into_iter().next())
    }
}

// ---- the pump -----------------------------------------------------------

/// Wait on the turn in flight, or forever when there is none.
/// The conversation as the model has it, for a driver to persist.
///
/// The system prompt is prepended, even though it lives in the prompt registry
/// rather than in the log. A snapshot is "what the model saw", and a driver that
/// persists one and resumes from it would otherwise come back with the system
/// message gone. A differential run against `atomcode-coding` caught exactly
/// that: it produced `user, assistant` where the reference produced
/// `system, …, user, assistant`.
///
/// One coalesced system message rather than the reference's several: the stack
/// already coalesces consecutive system messages on the wire, because some
/// providers only honour the first.
fn send_snapshot(ctx: &Context, events: &mpsc::UnboundedSender<AgentEvent>) {
    let Some(log) = ctx.service::<SessionSvc>() else {
        return;
    };
    let mut messages = Vec::new();
    if let Some(prompts) = ctx.service::<crate::seams::SystemPromptSvc>() {
        let system = prompts.render();
        if !system.is_empty() {
            messages.push(atomcode_kernel::message::Message::system(system));
        }
    }
    messages.extend(log.derive_messages());
    let _ = events.send(AgentEvent::Snapshot {
        snapshot: SessionSnapshot::new(messages),
    });
}

async fn finished(turn: &mut Option<tokio::task::JoinHandle<()>>) {
    match turn {
        Some(handle) => {
            let _ = handle.await;
        }
        None => std::future::pending::<()>().await,
    }
}

fn spawn_turn(
    driver: Arc<dyn crate::seams::AgentLoop>,
    agent: Arc<Agent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        driver.drive(&agent).await;
    })
}

/// What the pump woke up for.
enum Woke {
    Command(Option<AgentCommand>),
    TurnDone,
}

/// Translate commands until the driver hangs up.
///
/// Turns run on their own task so a cancel — or a second message, which folds
/// into the turn already running — is processed while the model is streaming.
/// A pump that blocked on the turn could not deliver the one command whose
/// entire purpose is to interrupt it.
async fn pump(
    ctx: Context,
    agent: Arc<Agent>,
    asker: Arc<Asker>,
    events: mpsc::UnboundedSender<AgentEvent>,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
) {
    let Ok(driver) = ctx.require::<AgentLoopSvc>() else {
        let _ = events.send(AgentEvent::Error {
            message: "no `agent-loop` is mounted; nothing can run a turn".into(),
            http_status: None,
            code: None,
            retryable: Some(false),
        });
        return;
    };
    let mut turn: Option<tokio::task::JoinHandle<()>> = None;
    // Snapshot requests that arrived while a turn was running.
    //
    // A driver asks for a snapshot to persist the conversation; answering it
    // from a log the running turn has not finished appending to hands back a
    // conversation that never existed. The reference engine answers after the
    // turn, and a differential run caught this: it replied instantly with
    // `messages=0` where the reference replied with `messages=4`.
    let mut snapshots_waiting = 0usize;
    let mut compactions_waiting: Vec<Option<String>> = Vec::new();

    loop {
        let woke = tokio::select! {
            command = commands.recv() => Woke::Command(command),
            _ = finished(&mut turn) => Woke::TurnDone,
        };

        let command = match woke {
            Woke::TurnDone => {
                turn = None;
                for focus in std::mem::take(&mut compactions_waiting) {
                    compact(&ctx, &events, focus).await;
                }
                for _ in 0..std::mem::take(&mut snapshots_waiting) {
                    send_snapshot(&ctx, &events);
                }
                // A message that arrived after the loop decided it was done
                // starts the next turn rather than waiting for one.
                if agent.inbox().has_waking_input() {
                    turn = Some(spawn_turn(driver.clone(), agent.clone()));
                }
                continue;
            }
            // The driver hung up: stop, and stop the turn with it.
            Woke::Command(None) => break,
            Woke::Command(Some(command)) => command,
        };

        match command {
            AgentCommand::SendMessage { text, images } => {
                agent.send_full(text, MessageOrigin::User, images);
            }
            // Context first, then the prompt: one command, one turn. The
            // context is model-visible and logged as the harness's, so a
            // transcript never shows the user saying it.
            AgentCommand::SendMessageWithContext {
                text,
                images,
                context,
            } => {
                agent.inject(context, crate::session::InjectionOrigin::Continuation);
                agent.send_full(text, MessageOrigin::User, images);
            }
            AgentCommand::SendSyntheticMessage { text } => {
                agent.send_from(text, MessageOrigin::Harness);
            }
            AgentCommand::Respond { id, value } => {
                asker.answer(id, value);
                continue;
            }
            AgentCommand::Cancel => {
                agent.cancel();
                // And release anything parked on an answer. Cancelling is
                // cooperative: a tool blocked on an approval nobody will now
                // give never reaches a point where it can observe the token,
                // so the turn hangs until the driver gives up. The shutdown
                // path already did both in this order and said why; the cancel
                // path only did the first half, and a liveness scenario in the
                // differential rig sat on it for the full twenty seconds.
                //
                // A pending approval becomes a refusal, which is the right
                // reading: the person asked to stop, not to proceed.
                asker.refuse_all();
                continue;
            }
            AgentCommand::Snapshot => {
                // Queued while a turn is in flight; answered the moment it
                // ends. Answering now would describe a conversation that is
                // still being written.
                if turn.is_some() {
                    snapshots_waiting += 1;
                } else {
                    send_snapshot(&ctx, &events);
                }
                continue;
            }
            AgentCommand::Compact { focus } => {
                // Behind the turn, like a snapshot and for the same reason:
                // rewriting the conversation while a round is mid-flight
                // compacts a history the turn is still appending to.
                if turn.is_some() {
                    compactions_waiting.push(focus);
                } else {
                    compact(&ctx, &events, focus).await;
                }
                continue;
            }
            AgentCommand::Shutdown => break,
            // `#[non_exhaustive]`: a command this harness has no answer for is
            // ignored rather than guessed at.
            _ => continue,
        }

        // Everything that falls through here queued work. A turn already
        // running claims it at its next step — that is what steering is — so a
        // second one must not be started.
        if turn.is_none() {
            turn = Some(spawn_turn(driver.clone(), agent.clone()));
        }
    }

    // On the way out: stop the turn, then release anything blocked on an answer
    // that is never coming. The other order deadlocks — a tool waiting on
    // approval never observes the cancel.
    agent.cancel();
    asker.close();
    if let Some(handle) = turn.take() {
        let _ = handle.await;
    }
}

async fn compact(ctx: &Context, events: &mpsc::UnboundedSender<AgentEvent>, focus: Option<String>) {
    let trigger = CompactTrigger::Manual {
        focus: focus.clone(),
    };
    let _ = events.send(AgentEvent::CompactionStarted {
        trigger: trigger.clone(),
    });
    let (Some(log), Some(compaction)) =
        (ctx.service::<SessionSvc>(), ctx.service::<CompactionSvc>())
    else {
        // Refused, not failed: nothing is mounted that could compact, and the
        // history is byte-identical.
        let _ = events.send(AgentEvent::Compacted {
            trigger,
            epoch: 0,
            removed: 0,
            bytes_before: 0,
            bytes_after: 0,
            committed: false,
            snapshot: None,
        });
        return;
    };
    let before = log.len();
    let decision = compaction.compact(&log).await;
    let committed = match decision {
        Some(decision) => {
            crate::session::apply_compaction(ctx, &log, decision);
            true
        }
        None => false,
    };
    let _ = events.send(AgentEvent::Compacted {
        trigger,
        epoch: 0,
        removed: if committed { before } else { 0 },
        bytes_before: before,
        bytes_after: log.len(),
        committed,
        snapshot: committed.then(|| SessionSnapshot::new(log.derive_messages())),
    });
}

// ---- the row ------------------------------------------------------------

struct HandleFrontEnd {
    handle: Mutex<Option<AgentHandle>>,
    /// Fires when the pump stops, so `run` returns at the same moment the
    /// handle's task does without either of them owning the join handle.
    done: Mutex<Option<oneshot::Receiver<()>>>,
    initial: Mutex<Option<mpsc::UnboundedSender<AgentCommand>>>,
}

impl AgentHandleSource for HandleFrontEnd {
    fn take(&self) -> Option<AgentHandle> {
        self.handle.lock().expect("handle poisoned").take()
    }
}

#[async_trait]
impl UserInterface for HandleFrontEnd {
    fn describe(&self) -> String {
        "a driver on the other end of an AgentHandle".into()
    }

    async fn run(&self, _ctx: &Context, initial: Option<String>) -> Result<(), String> {
        if let (Some(text), Some(commands)) = (
            initial,
            self.initial.lock().expect("commands poisoned").take(),
        ) {
            let _ = commands.send(AgentCommand::SendMessage {
                text,
                images: Vec::new(),
            });
        }
        let done = self
            .done
            .lock()
            .expect("done poisoned")
            .take()
            .ok_or("this front end can only be run once")?;
        let _ = done.await;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct HandleRow {
    /// How long a question waits before it is treated as unanswered. A driver
    /// that has gone away must not hold a turn open forever.
    #[serde(default = "default_ask_timeout")]
    ask_timeout_secs: u64,
}

fn default_ask_timeout() -> u64 {
    300
}

impl Default for HandleRow {
    fn default() -> Self {
        Self {
            ask_timeout_secs: default_ask_timeout(),
        }
    }
}

pub struct AgentHandlePlugin;

#[async_trait]
impl Plugin for AgentHandlePlugin {
    fn name(&self) -> &'static str {
        "ui-handle"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["agents", "agent-loop", "sessions"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // `approval` because the gate resolves the policy per call rather than
        // capturing it — the same reason the static approval row declares it.
        &["tools", "llm", "compaction", "approval"]
    }
    fn provides(&self) -> &'static [&'static str] {
        // A driver renders prompts and answers them, so it fills the asking
        // seams too — and the standalone rows that would otherwise claim them
        // must stand down.
        &["ui", "agent-handle", "user-questions", "approval"]
    }
    fn description(&self) -> &'static str {
        "drive this harness through the AgentHandle protocol the shipped UIs speak"
    }

    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: HandleRow = if config.is_null() {
            HandleRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };

        let (command_tx, command_rx) = mpsc::unbounded_channel::<AgentCommand>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();

        let asker = Arc::new(Asker::new(
            event_tx.clone(),
            Duration::from_secs(row.ask_timeout_secs),
        ));
        // Both asking seams, filled before anything mounts on top of them: a
        // consumer that resolves `approval` during its own `apply` must find it
        // already there.
        let _ = ctx
            .provide::<UserQuestionsSvc>(asker.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<crate::seams::ApprovalSvc>(asker.clone())
            .map_err(|e| e.to_string())?;
        // Filling the slot is not gating. Every `approval` provider registers
        // the gate that consults it, and one that forgot would leave a tree
        // where the policy exists, resolves, and is never asked.
        let _ = ctx.on_waterfall::<crate::events::ToolsExecute>(
            Arc::new(super::policy::ApprovalGate { ctx: ctx.clone() }),
            false,
        );

        // One agent, created here rather than on the first message, so the
        // registry and any `agent/created` observer see it before the driver
        // can send anything.
        let agents = ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
        let agent = agents.create(ctx);
        ctx.emit::<AgentCreated>(&AgentInfo { id: agent.id() });

        let session: Option<Arc<SessionLog>> = ctx.service::<SessionSvc>();
        let session_id = session
            .as_ref()
            .map(|s| s.id().to_string())
            .unwrap_or_default();
        let projector = Arc::new(Mutex::new(Projector {
            tools: ctx.service::<ToolsSvc>(),
            ctx_window: ctx
                .service::<LlmSvc>()
                .map(|p| p.context_window())
                .unwrap_or(0),
            batch: None,
            last_prompt_tokens: 0,
            said_this_turn: 0,
        }));

        let out = event_tx.clone();
        let fold = projector.clone();
        let stream = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            // One handle, one conversation. A delegated child commits to its
            // own log and this listener is above both.
            if committed.session != session_id {
                return;
            }
            let projected = fold
                .lock()
                .expect("projector poisoned")
                .project(&committed.event);
            for event in projected {
                let _ = out.send(event);
            }
        });

        let (done_tx, done_rx) = oneshot::channel();
        let pump_ctx = ctx.clone();
        let pump_agent = agent.clone();
        let pump_asker = asker.clone();
        let pump_events = event_tx.clone();
        let task = tokio::spawn(async move {
            pump(pump_ctx, pump_agent, pump_asker, pump_events, command_rx).await;
            // The listener holds a clone of the sender; revoking it is what
            // lets the event channel close, so a driver reading to the end sees
            // the end. Dropping only the local handles would hang it forever.
            stream.dispose();
            let _ = done_tx.send(());
        });

        let front = Arc::new(HandleFrontEnd {
            handle: Mutex::new(Some(AgentHandle {
                commands: command_tx.clone(),
                events: event_rx,
                task,
            })),
            done: Mutex::new(Some(done_rx)),
            initial: Mutex::new(Some(command_tx)),
        });
        let _ = ctx
            .provide::<AgentHandleSvc>(front.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx.provide::<UiSvc>(front).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The projection, addressable on its own.
///
/// A caller that already has a session's events — a resumed log, a stored
/// transcript, a test — replays them into the same driver events a live turn
/// produces, because it is the same fold.
pub fn replay(events: &[SessionEvent], ctx_window: u32) -> Vec<AgentEvent> {
    let mut projector = Projector {
        tools: None,
        ctx_window,
        batch: None,
        last_prompt_tokens: 0,
        said_this_turn: 0,
    };
    events
        .iter()
        .flat_map(|event| projector.project(event))
        .collect()
}

/// Ask a driver a question through a handle's protocol, for a caller that has
/// the handle rather than the context.
pub fn question_payload(question: &str, options: &[String]) -> Value {
    json!({
        "header": "Question",
        "question": question,
        "mode": if options.is_empty() { "text" } else { "single" },
        "options": options
            .iter()
            .map(|label| json!({ "label": label }))
            .collect::<Vec<_>>(),
        "custom": true,
    })
}
