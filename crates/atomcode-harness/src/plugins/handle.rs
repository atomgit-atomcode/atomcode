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
use crate::events::{
    AgentChange, AgentCreated, AgentInfo, AgentRemoved as AgentRemovedEvent, AgentStatusChanged,
    InboxInserted, SessionEventCommitted,
};
use crate::seams::{
    AgentHandleSource, AgentHandleSvc, AgentLoopSvc, AgentsSvc, ApprovalPolicy, CompactionSvc,
    Decision, LlmSvc, SessionSvc, ToolBox, ToolsSvc, UiSvc, UserInterface, UserQuestions,
    UserQuestionsSvc,
};
use crate::session::{Committed, SessionEvent};

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
    /// The round now running answers a nudge the harness wrote. What the model
    /// says back is not shown: it is answering a note the person never sent. What
    /// it DOES still is — the calls it makes and their results project as always.
    answering_a_nudge: bool,
    /// The last round a `Usage` was reported for. A round the provider reported
    /// no usage for is still a round, and a driver counting rounds by `Usage`
    /// must hear of it.
    usage_round: Option<(u64, u32)>,
    /// A `/compact` is committing its own compaction, and reports it itself —
    /// with the trigger it had, the numbers it measured and the snapshot. The
    /// fold's generic report of the same commit would be a second, auto-labelled
    /// one.
    manual_compaction: Arc<std::sync::atomic::AtomicBool>,
}

impl Projector {
    // `mounted()` lived here: a guess at whether a named tool would actually
    // dispatch, so an unmounted one was not announced as started. The guess is
    // gone because the question is now answered rather than predicted —
    // `SessionEvent::ToolStarted` is committed past the catalog lookup, so a
    // tool nobody mounted commits nothing and there is nothing to suppress.

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
            SessionEvent::TurnStart { turn } => {
                self.said_this_turn = 0;
                vec![AgentEvent::TurnStarted { turn: Some(*turn) }]
            }

            SessionEvent::AssistantChunk {
                delta, reasoning, ..
            } => {
                // Answering a nudge nobody wrote: the words are not the person's
                // business, the actions are. Logged all the same — this decides
                // what is SHOWN, never what is kept.
                if self.answering_a_nudge {
                    Vec::new()
                } else if *reasoning {
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
                // The provider said nothing about tokens this round. The round
                // happened all the same: report it, with what is known — no
                // counts, and the context as last measured.
                if self.usage_round != Some((*turn, *round)) {
                    self.usage_round = Some((*turn, *round));
                    out.push(AgentEvent::Usage(MessageMeta {
                        ctx_window: self.ctx_window,
                        used_tokens: self.last_prompt_tokens,
                        utilization: if self.ctx_window == 0 {
                            0.0
                        } else {
                            self.last_prompt_tokens as f32 / self.ctx_window as f32
                        },
                        round: *round,
                        turn_id: *turn,
                        request_id: *round as u64,
                        ..Default::default()
                    }));
                }
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
                // No `ToolStarted` here. This fact says the model ASKED, and
                // it is committed before approval, plan mode, the workspace
                // gates or a user's hook have had a say — so announcing a start
                // from it showed every refused call as one that began and
                // instantly failed, and showed a write as under way before the
                // person was asked to allow it. `SessionEvent::ToolStarted`,
                // committed where the call actually runs, is the fact for that.
                out
            }

            SessionEvent::ToolStarted { call, .. } => {
                vec![AgentEvent::ToolStarted { call: call.clone() }]
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

            SessionEvent::StepEnd { .. } => {
                // Whatever the next round says, it is the model's own again.
                self.answering_a_nudge = false;
                match self.batch.take() {
                    Some(batch) => vec![AgentEvent::ToolBatchCompleted {
                        batch_id: batch.id,
                        ok: batch.ok,
                        total: batch.total,
                        elapsed_ms: batch.started.elapsed().as_millis() as u64,
                    }],
                    None => Vec::new(),
                }
            }

            SessionEvent::Usage { turn, round, usage } => {
                self.last_prompt_tokens = usage.prompt;
                self.usage_round = Some((*turn, *round));
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
            SessionEvent::Compacted { .. }
                if self
                    .manual_compaction
                    .load(std::sync::atomic::Ordering::SeqCst) =>
            {
                Vec::new()
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
                    // A warning in the kernel protocol too: nothing is being
                    // recovered, the person is being told to ask for the rest.
                    crate::session::NoticeKind::OutputLeftCutOff => {
                        AgentEvent::Warning(detail.clone())
                    }
                }]
            }

            SessionEvent::RateLimitPaused { pause, .. } => vec![AgentEvent::RateLimited {
                reset_at_display: pause.reset_at_display.clone(),
                reset_label: pause.reset_label.clone(),
                secs_until_reset: pause.secs_until_reset,
                auto_resuming: false,
                server_message: pause.server_message.clone(),
            }],
            SessionEvent::PolicyIntervention { intervention, .. } => {
                vec![AgentEvent::PolicyIntervention {
                    intervention: intervention.clone(),
                }]
            }

            SessionEvent::TurnEnd { turn, stop, error } => {
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
                    turn: Some(*turn),
                    reason: *stop,
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
            SessionEvent::UserMessage { turn, text, images } => {
                if self.said_this_turn == 0 {
                    self.said_this_turn = 1;
                    Vec::new()
                } else {
                    self.said_this_turn += 1;
                    vec![AgentEvent::Steered {
                        turn: Some(*turn),
                        count: 1,
                        inputs: vec![atomcode_kernel::event::SteeredInput {
                            text: text.clone(),
                            images: images.clone(),
                        }],
                    }]
                }
            }
            // Model-visible context the person did not type. Dropping it here
            // was the bug this variant exists to fix: the fact reached the log
            // and the model's request and stopped, so a team member's report was
            // invisible to the person it was being reported to.
            SessionEvent::Injected { text, origin, .. } => {
                self.answering_a_nudge =
                    matches!(origin, crate::session::InjectionOrigin::InternalNudge);
                use crate::session::InjectionOrigin as In;
                use atomcode_kernel::event::ContextSource as Out;
                vec![AgentEvent::ContextAdded {
                    text: text.clone(),
                    source: match origin {
                        In::Peer { from } => Out::Peer { from: from.clone() },
                        In::Memory => Out::Memory,
                        In::Reminder => Out::Reminder,
                        In::Continuation => Out::Continuation,
                        In::InternalNudge => Out::Continuation,
                        In::CompactionSummary => Out::CompactionSummary,
                    },
                }]
            }

            SessionEvent::StepStart { .. }
            | SessionEvent::RequestHeader { .. }
            | SessionEvent::Titled { .. }
            // The ladder that stubs says so itself, as a notice; the fact is for
            // the log and the next request, not for the screen.
            | SessionEvent::ToolResultsStubbed { .. }
            // A question was put, and answered: a card in the log, drawn by the
            // front end that asked for it out of the same fold as every other
            // block. The kernel protocol has no vocabulary for a question, and
            // the answer the model sees travels its own way — as the tool's
            // result.
            | SessionEvent::Asked { .. }
            | SessionEvent::Answered { .. }
            // The driver already knows: it sent the cancel, and the turn's end
            // says `Cancelled`. What changed is the model's view, which is the
            // log's business.
            | SessionEvent::Interrupted { .. } => Vec::new(),
            // `SessionEvent` is the kernel's and `non_exhaustive` (`docs/adr/0024`
            // §6), so this match can no longer refuse to compile when a fact is
            // added. The list above stays explicit on purpose: every fact the
            // harness knows is decided here, by name. A fact added later is
            // silent until someone adds it to the list.
            _ => Vec::new(),
        }
    }
}

// ---- inward: asking the driver ------------------------------------------

/// The half that asks. Fills both the `approval` and `user-questions` seams,
/// because a driver that can render a prompt can answer either.
struct Asker {
    /// The row's own realm. Held for the one thing the asking half does that is
    /// not a round-trip: writing the question and its answer into the session's
    /// log, so a driver that was not attached when it happened can still find
    /// out what was decided.
    ctx: Context,
    /// Dropped on shutdown. The asker outlives the pump — it is a service in
    /// the tree — so a sender it held forever would keep the event channel
    /// open forever, and a driver reading to the end would never reach one.
    events: Mutex<Option<mpsc::UnboundedSender<AgentEvent>>>,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    /// Calls the driver said to stop asking about, as `{tool}::{scope}`.
    ///
    /// The SCOPE, not the exact argument bytes. "Always allow" is a statement
    /// about a set of calls, and which set is the gate's to decide — that is
    /// what [`Tool::always_grant_scope`] is for: a write grants its directory, a
    /// bash command grants that command, a tool with nothing to say grants
    /// itself tool-wide. Keying on the bytes quietly narrowed every one of those
    /// to "this one call", so a person who answered "always allow writes here"
    /// was asked again about the very next file in the same directory.
    ///
    /// Same key shape as `policy_rows::AskingPolicy`, deliberately: two
    /// implementations of one seam that remember different things are two
    /// different products.
    granted: Mutex<HashSet<String>>,
    timeout: Duration,
}

impl Asker {
    fn new(ctx: Context, events: mpsc::UnboundedSender<AgentEvent>, timeout: Duration) -> Self {
        Self {
            ctx,
            events: Mutex::new(Some(events)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            granted: Mutex::new(HashSet::new()),
            timeout,
        }
    }

    /// Send the driver an event outside any round trip, while it is connected.
    fn emit(&self, event: AgentEvent) {
        if let Some(events) = self.events.lock().expect("events poisoned").as_ref() {
            let _ = events.send(event);
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
            None => Err(mpsc::error::SendError(AgentEvent::TurnStarted {
                turn: None,
            })),
        };
        // Nobody on the other end: a refusal, not a wait.
        if sent.is_err() {
            self.pending.lock().expect("pending poisoned").remove(&id);
            return None;
        }
        // Zero is "wait for the answer": a person at the terminal is not auto-denied
        // for stepping away, which is what a host with nobody bounded asks for.
        let answered = if self.timeout.is_zero() {
            Ok(rx.await)
        } else {
            tokio::time::timeout(self.timeout, rx).await
        };
        match answered {
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
        // `NEVER_GRANT` is a gate saying "this one may never be remembered" — a
        // write to a credential file, or a hook whose whole point was to stop
        // and ask every time. Then there is nothing to look up and nothing to
        // record, whatever the driver answers. NOT the empty scope, which
        // several tools already use to mean the opposite: a tool-wide grant.
        let scope = tool.always_grant_scope(&call.arguments);
        let grantable = scope != crate::seams::NEVER_GRANT;
        let key = format!("{}::{scope}", tool.name());
        // A host that keeps the session's grants past this tree (the coding
        // runtime rebuilds it on undo) provides them; otherwise they live here.
        let kept = self.ctx.service::<crate::seams::GrantsSvc>();
        let remembered = match &kept {
            Some(store) => store.is_granted(&key),
            None => self.granted.lock().expect("grants poisoned").contains(&key),
        };
        if grantable && remembered {
            return Decision::Allow;
        }
        let question = crate::seams::Question::approval(
            tool.name(),
            &call.arguments,
            grantable.then_some(scope.as_str()),
            crate::agent::current_member_name(&self.ctx),
        );
        // Written down around the round-trip, not instead of it: what goes over
        // the wire stays the driver's own `ApprovalRequest`/`PermissionDecision`
        // contract — the one every shipped front end already speaks — and the
        // question above is the record of it. A session a client can rejoin is
        // the point; a new wire shape for it would be a different change.
        crate::agent::record_asked(&self.ctx, &question);
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
        let decision = PermissionDecision::from_value(&answer.unwrap_or(Value::Null));
        let value = match decision {
            PermissionDecision::AllowOnce => crate::seams::ANSWER_ALLOW,
            PermissionDecision::AllowAlways => crate::seams::ANSWER_ALWAYS,
            PermissionDecision::Deny => crate::seams::ANSWER_DENY,
        };
        // The answer as a value the log can hold, not the decision: `allow` is
        // what the person chose, and what the policy makes of it is this row's
        // business. Logging the decision would write this row's vocabulary into
        // a record that outlives it.
        crate::agent::record_answered(&self.ctx, Some(value.to_string()));
        match decision {
            PermissionDecision::AllowOnce => Decision::Allow,
            PermissionDecision::AllowAlways => {
                // An "always" for something un-grantable is honoured as an
                // allow-once rather than refused: the person did say yes. It is
                // simply not remembered, which is the whole meaning of
                // `NEVER_GRANT`.
                if grantable {
                    match &kept {
                        Some(store) => store.grant(&key),
                        None => {
                            self.granted.lock().expect("grants poisoned").insert(key);
                        }
                    }
                }
                Decision::Allow
            }
            PermissionDecision::Deny => {
                Decision::Deny(format!("`{}` was not approved", tool.name()))
            }
        }
    }
}

#[async_trait]
impl UserQuestions for Asker {
    fn describe(&self) -> String {
        "the connected driver".into()
    }

    async fn ask(&self, question: &crate::seams::Question) -> Option<String> {
        let request = UserInputRequest {
            // Who is asking belongs in the header, where a driver draws it
            // without having to parse the sentence for a name.
            header: match &question.asker {
                Some(who) => format!("Question · {who}"),
                None => "Question".into(),
            },
            question: question.prompt.clone(),
            mode: if question.options.is_empty() {
                UserInputMode::Text
            } else {
                UserInputMode::Single
            },
            // `label` carries the value, because that is what comes back and
            // what the caller compares against; the wording rides along as the
            // description, for a driver with nothing better to show.
            options: question
                .options
                .iter()
                .map(|answer| UserInputOption {
                    label: answer.value.clone(),
                    description: Some(answer.label.clone()),
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

/// The half of a driver that holds the questions.
///
/// The pump needs exactly three things from it: to route an answer that came
/// back over the wire, to refuse everything pending when the person cancels
/// (a tool blocked on an approval nobody will now give never reaches a point
/// where it can observe the cancel), and to close on shutdown. The handle's own
/// [`Asker`] asks over the event channel; a screen asks by drawing. Both fit.
pub trait Answers: Send + Sync {
    /// Deliver an answer. `false` means nobody was waiting for it.
    fn answer(&self, id: RequestId, value: Value) -> bool;
    /// Every pending question, refused at once.
    fn refuse_all(&self);
    /// No more questions; whatever is still waiting is refused.
    fn close(&self);
}

impl Answers for Asker {
    fn answer(&self, id: RequestId, value: Value) -> bool {
        Asker::answer(self, id, value)
    }
    fn refuse_all(&self) {
        Asker::refuse_all(self)
    }
    fn close(&self) {
        Asker::close(self)
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
    /// Something reached the inbox from somewhere other than a command — a
    /// peer, a timer, a goal controller.
    Inbox,
}

/// Translate commands until the driver hangs up.
///
/// Turns run on their own task so a cancel — or a second message, which folds
/// into the turn already running — is processed while the model is streaming.
/// A pump that blocked on the turn could not deliver the one command whose
/// entire purpose is to interrupt it.
/// Sessions this handle streams facts for, each with the highest seq it has
/// sent. Shared by the fact listener and the pump, and held across a
/// subscription's catch-up, so history and live facts meet with no gap and no
/// repeat.
type Subscriptions = Arc<Mutex<HashMap<String, Subscription>>>;

/// What one subscribed session's subscriber has been sent so far.
struct Subscription {
    /// The last fact.
    high: crate::session::SeqNo,
    /// The members it has been told joined and not yet told left. Only these
    /// are reported on, so a member is never heard of before it is added or
    /// after it is removed, and never added twice.
    members: HashSet<String>,
}

/// Send the picture of a session's members a new subscriber starts from: each
/// member described, then where it stands.
fn announce_member(
    events: &mpsc::UnboundedSender<AgentEvent>,
    subscription: &mut Subscription,
    member: &Agent,
) {
    if subscription.members.insert(member.session_id().to_string()) {
        let _ = events.send(AgentEvent::AgentAdded {
            description: Box::new(member.describe()),
        });
        let _ = events.send(AgentEvent::StatusChanged {
            session: member.session_id().to_string(),
            status: member.status(),
        });
    }
}

async fn pump(
    ctx: Context,
    agent: Arc<Agent>,
    asker: Arc<dyn Answers>,
    events: mpsc::UnboundedSender<AgentEvent>,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
    manual_compaction: Arc<std::sync::atomic::AtomicBool>,
    subscriptions: Subscriptions,
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
    // Snapshots and compactions act on this agent's log, which lives in its
    // realm; the tree's context would not find it.
    let ctx = agent.ctx().clone();
    // Work that arrives without a command still has to run. The listener is
    // on the agent's own context; a child's inbox event reaches here too
    // (visibility is upward), so it is filtered to this agent.
    let (woke_tx, mut woke_rx) = mpsc::unbounded_channel::<()>();
    let me = agent.id();
    let wake = ctx.on_emit::<InboxInserted>(move |info: &AgentInfo| {
        if info.id == me {
            let _ = woke_tx.send(());
        }
    });
    // A message a driver asked a receipt for is accepted when a turn claims it
    // — not when it is queued, because only then is it known whether it starts
    // a turn or joins the one running (`docs/adr/0021` §7).
    let receipts = events.clone();
    let claimed =
        ctx.on_emit::<crate::events::InputClaimed>(move |input: &crate::events::ClaimedInput| {
            if input.agent == me {
                let _ = receipts.send(AgentEvent::Accepted {
                    command: input.receipt.clone(),
                    turn: Some(input.turn),
                    steered: input.steered,
                });
            }
        });
    // Anything that landed before the listener existed: this task is spawned
    // at mount and a message can reach the inbox before it runs. Listener
    // first, then the look — so nothing falls between them.
    let mut turn: Option<tokio::task::JoinHandle<()>> = if agent.inbox().has_waking_input() {
        Some(spawn_turn(driver.clone(), agent.clone()))
    } else {
        None
    };
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
            _ = woke_rx.recv() => Woke::Inbox,
        };

        let command = match woke {
            Woke::TurnDone => {
                turn = None;
                for focus in std::mem::take(&mut compactions_waiting) {
                    compact(&ctx, &events, focus, &manual_compaction).await;
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
            // The same rule as after a turn: a message starts one, an
            // injection alone waits for one. A command's own message arrives
            // here as well, harmlessly — the turn it started is already running.
            Woke::Inbox => {
                if turn.is_none() && agent.inbox().has_waking_input() {
                    turn = Some(spawn_turn(driver.clone(), agent.clone()));
                }
                continue;
            }
            // The driver hung up: stop, and stop the turn with it.
            Woke::Command(None) => break,
            Woke::Command(Some(command)) => command,
        };

        // A tagged command is the command it carries, plus an id the driver wants
        // echoed on its receipt. Messages are receipted when a turn claims them;
        // everything else is answered here, on the spot.
        let (receipt, command) = match command {
            AgentCommand::Tagged { id, command } => (Some(id), command.untagged()),
            // A catalog command carries its own receipt.
            AgentCommand::Invoke { ref id, .. } => (Some(id.clone()), command),
            other => (None, other),
        };
        let accept = |turn: Option<u64>| {
            if let Some(id) = &receipt {
                let _ = events.send(AgentEvent::Accepted {
                    command: id.clone(),
                    turn,
                    steered: false,
                });
            }
        };
        let reject = |error: atomcode_kernel::event::CommandError| {
            if let Some(id) = &receipt {
                let _ = events.send(AgentEvent::Rejected {
                    command: id.clone(),
                    error,
                });
            }
        };

        match command {
            AgentCommand::SendMessage { text, images } => {
                agent.send_receipted(text, MessageOrigin::User, images, receipt.clone());
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
                agent.send_receipted(text, MessageOrigin::User, images, receipt.clone());
            }
            AgentCommand::SendSyntheticMessage { text } => {
                agent.send_receipted(text, MessageOrigin::Harness, Vec::new(), receipt.clone());
            }
            AgentCommand::Respond { id, value } => {
                if asker.answer(id, value) {
                    accept(None);
                } else {
                    reject(atomcode_kernel::event::CommandError::StaleQuestion);
                }
                continue;
            }
            AgentCommand::Cancel => {
                if turn.is_some() {
                    accept(Some(agent.session().current_turn()));
                } else {
                    reject(atomcode_kernel::event::CommandError::NotRunning);
                }
                // The driver's cancel is a person's: the turn's end records it.
                agent.interrupt();
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
                accept(None);
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
                accept(None);
                // Behind the turn, like a snapshot and for the same reason:
                // rewriting the conversation while a round is mid-flight
                // compacts a history the turn is still appending to.
                if turn.is_some() {
                    compactions_waiting.push(focus);
                } else {
                    compact(&ctx, &events, focus, &manual_compaction).await;
                }
                continue;
            }
            AgentCommand::Subscribe { session, from } => {
                // Its own session, or one it can reach by id — a team member's.
                let agents = ctx.service::<AgentsSvc>();
                let target = if session == agent.session_id() {
                    Some(agent.clone())
                } else {
                    agents
                        .as_ref()
                        .and_then(|agents| agents.by_session(&session))
                };
                match target {
                    None => reject(atomcode_kernel::event::CommandError::NotFound),
                    Some(target) => {
                        accept(None);
                        // Held while the picture and the history are sent: a
                        // fact committed, a member added or a status moved
                        // meanwhile waits in its listener and then compares
                        // against what is recorded here.
                        let mut subscribed = subscriptions.lock().expect("subscriptions poisoned");
                        let mut subscription = Subscription {
                            high: from.saturating_sub(1),
                            members: HashSet::new(),
                        };
                        let _ = events.send(AgentEvent::Described {
                            description: Box::new(target.describe()),
                        });
                        let _ = events.send(AgentEvent::StatusChanged {
                            session: session.clone(),
                            status: target.status(),
                        });
                        for member in agents.iter().flat_map(|agents| agents.list()) {
                            if member.parent() == Some(session.as_str()) {
                                announce_member(&events, &mut subscription, &member);
                            }
                        }
                        for logged in target.session().events() {
                            if logged.seq >= from {
                                subscription.high = logged.seq;
                                let _ = events.send(AgentEvent::Fact(Box::new(Committed {
                                    session: session.clone(),
                                    seq: logged.seq,
                                    event: logged.event,
                                })));
                            }
                        }
                        subscribed.insert(session, subscription);
                    }
                }
                continue;
            }
            AgentCommand::Unsubscribe { session } => {
                subscriptions
                    .lock()
                    .expect("subscriptions poisoned")
                    .remove(&session);
                accept(None);
                continue;
            }
            // Nothing has put a command in the catalog yet — the registry
            // capability rows register into comes with the team (plan 4.4) — so
            // there is no command by any name here.
            AgentCommand::Invoke { .. } => {
                reject(atomcode_kernel::event::CommandError::NotFound);
                continue;
            }
            AgentCommand::Shutdown => {
                accept(None);
                break;
            }
            // `#[non_exhaustive]`: a command this harness has no answer for is
            // refused rather than guessed at.
            _ => {
                reject(atomcode_kernel::event::CommandError::Unsupported);
                continue;
            }
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
    wake.dispose();
    claimed.dispose();
    if let Some(handle) = turn.take() {
        let _ = handle.await;
    }
}

async fn compact(
    ctx: &Context,
    events: &mpsc::UnboundedSender<AgentEvent>,
    focus: Option<String>,
    reporting: &std::sync::atomic::AtomicBool,
) {
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
    // Measured on what the model sees, which is what a driver reports and what
    // its token estimate is computed from — not on the log, which compaction
    // never shrinks.
    let measure = |messages: &[atomcode_kernel::message::Message]| {
        (
            messages.len(),
            messages.iter().map(|m| m.text.len()).sum::<usize>(),
        )
    };
    let (count_before, bytes_before) = measure(&log.derive_messages());
    let decision = compaction.compact_requested(&log, focus.as_deref()).await;
    let committed = match decision {
        Some(decision) => {
            reporting.store(true, std::sync::atomic::Ordering::SeqCst);
            crate::session::apply_compaction(ctx, &log, decision);
            reporting.store(false, std::sync::atomic::Ordering::SeqCst);
            true
        }
        None => false,
    };
    let after = log.derive_messages();
    let (count_after, bytes_after) = measure(&after);
    let _ = events.send(AgentEvent::Compacted {
        trigger,
        epoch: 0,
        removed: count_before.saturating_sub(count_after),
        bytes_before,
        bytes_after,
        committed,
        // The conversation as a snapshot holds it: with the system prompt at its
        // head, the way `send_snapshot` answers.
        snapshot: committed.then(|| {
            let mut messages = Vec::new();
            if let Some(prompts) = ctx.service::<crate::seams::SystemPromptSvc>() {
                let system = prompts.render();
                if !system.is_empty() {
                    messages.push(atomcode_kernel::message::Message::system(system));
                }
            }
            messages.extend(after);
            SessionSnapshot::new(messages)
        }),
    });
}

// ---- what a running tool reaches ------------------------------------------

/// The driven agent's tools talk to its driver: progress as `ToolProgress`, a
/// question as a `Request` answered like any other.
///
/// Both go through the asker, never through a sender of its own: the asker gives
/// its sender up when the pump stops, and a service holding another one would
/// keep the driver's event stream open for as long as the tree lives.
struct HandleToolDriver {
    session: String,
    asker: Arc<Asker>,
}

impl crate::seams::ToolDriver for HandleToolDriver {
    fn progress(&self, session: &str, call_id: &str) -> atomcode_kernel::tool::ProgressSink {
        if session != self.session {
            return atomcode_kernel::tool::ProgressSink::noop();
        }
        let asker = self.asker.clone();
        let id = call_id.to_string();
        atomcode_kernel::tool::ProgressSink::with_source_id(
            call_id.to_string(),
            Arc::new(move |message| {
                asker.emit(AgentEvent::ToolProgress {
                    call_id: id.clone(),
                    message,
                });
            }),
        )
    }

    fn requester(&self, session: &str) -> Option<atomcode_kernel::request::Requester> {
        if session != self.session {
            return None;
        }
        let asker = self.asker.clone();
        Some(atomcode_kernel::request::Requester::from_fn(Arc::new(
            move |kind, payload| {
                let asker = asker.clone();
                Box::pin(async move { asker.request(&kind, payload).await.unwrap_or(Value::Null) })
            },
        )))
    }
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

/// One driver's channel pair, made before anything that needs a sending end.
///
/// Whoever asks questions over the wire needs `events` before the pump exists,
/// and the pump needs the receiving ends — so the pair is made first and handed
/// over whole.
pub struct Wire {
    pub commands: mpsc::UnboundedSender<AgentCommand>,
    pub events: mpsc::UnboundedSender<AgentEvent>,
    command_rx: mpsc::UnboundedReceiver<AgentCommand>,
    event_rx: mpsc::UnboundedReceiver<AgentEvent>,
}

pub fn wire() -> Wire {
    let (commands, command_rx) = mpsc::unbounded_channel::<AgentCommand>();
    let (events, event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    Wire {
        commands,
        events,
        command_rx,
        event_rx,
    }
}

/// A driver running: the handle to speak through, and the moment it hangs up.
pub struct Driven {
    pub handle: AgentHandle,
    /// The agent behind the handle, for a front end that lives in the same
    /// process and wants its log or its realm without going through the wire.
    pub agent: Arc<Agent>,
    /// Resolves once the pump has stopped: the turn is over and the asker is
    /// closed. What a front end waits on before it returns.
    pub done: oneshot::Receiver<()>,
}

/// Create one agent and drive it through the handle protocol.
///
/// This is the whole of what a driver-protocol front end does, addressable on
/// its own so that a front end living *in* the tree (the full-screen TUI) and
/// one on the far end of a channel (a daemon, a test) run the same pump: the
/// same steering, the same cancel that releases the asker, the same compaction
/// and snapshot ordering behind the turn. A second driver written beside this
/// one drifted on exactly those points, which is why there is no second one.
///
/// `answers` is whoever holds the questions — the handle's own [`Asker`], or a
/// screen that draws them.
pub async fn spawn(
    ctx: &Context,
    wire: Wire,
    answers: Arc<dyn Answers>,
    req: crate::agent::CreateAgent,
) -> Result<Driven, String> {
    let Wire {
        commands,
        events,
        command_rx,
        event_rx,
    } = wire;

    // One agent, created here rather than on the first message, so the
    // registry and any `agent/created` observer see it before the driver
    // can send anything. Its log comes with it.
    let agents = ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
    let agent = agents.create(ctx, req).await?;
    let session_id = agent.session_id().to_string();
    let manual_compaction = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let projector = Arc::new(Mutex::new(Projector {
        tools: ctx.service::<ToolsSvc>(),
        ctx_window: ctx
            .service::<LlmSvc>()
            .map(|p| p.context_window())
            .unwrap_or(0),
        batch: None,
        last_prompt_tokens: 0,
        said_this_turn: 0,
        answering_a_nudge: false,
        usage_round: None,
        manual_compaction: manual_compaction.clone(),
    }));

    let out = events.clone();
    let fold = projector.clone();
    let subscriptions: Subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let subscribed = subscriptions.clone();
    let stream = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
        // Facts for whoever subscribed to this session — its own or a member's.
        {
            let mut sessions = subscribed.lock().expect("subscriptions poisoned");
            if let Some(subscription) = sessions.get_mut(&committed.session) {
                if committed.seq > subscription.high {
                    subscription.high = committed.seq;
                    let _ = out.send(AgentEvent::Fact(Box::new(committed.clone())));
                }
            }
        }
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

    // A subscribed session's members, as they come and go, and where each
    // subscribed agent stands. Registered on the tree, which every agent's
    // realm announces up to.
    let members_out = events.clone();
    let members_seen = subscriptions.clone();
    let registry = agents.clone();
    let added = ctx.on_emit::<AgentCreated>(move |info: &AgentInfo| {
        let Some(member) = registry.get(info.id) else {
            return;
        };
        let Some(parent) = member.parent() else {
            return;
        };
        let mut sessions = members_seen.lock().expect("subscriptions poisoned");
        if let Some(subscription) = sessions.get_mut(parent) {
            announce_member(&members_out, subscription, &member);
        }
    });
    let removed_out = events.clone();
    let removed_seen = subscriptions.clone();
    let removed = ctx.on_emit::<AgentRemovedEvent>(move |gone: &AgentChange| {
        let Some(parent) = &gone.parent else {
            return;
        };
        let mut sessions = removed_seen.lock().expect("subscriptions poisoned");
        if let Some(subscription) = sessions.get_mut(parent) {
            if subscription.members.remove(&gone.session) {
                let _ = removed_out.send(AgentEvent::AgentRemoved {
                    session: gone.session.clone(),
                });
            }
        }
    });
    let status_out = events.clone();
    let status_seen = subscriptions.clone();
    let moved = ctx.on_emit::<AgentStatusChanged>(move |change: &AgentChange| {
        let sessions = status_seen.lock().expect("subscriptions poisoned");
        let subscribed = sessions.contains_key(&change.session)
            || change
                .parent
                .as_ref()
                .and_then(|parent| sessions.get(parent))
                .is_some_and(|subscription| subscription.members.contains(&change.session));
        if subscribed {
            let _ = status_out.send(AgentEvent::StatusChanged {
                session: change.session.clone(),
                status: change.status,
            });
        }
    });

    let (done_tx, done_rx) = oneshot::channel();
    let pump_ctx = ctx.clone();
    let pump_agent = agent.clone();
    let task = tokio::spawn(async move {
        pump(
            pump_ctx,
            pump_agent,
            answers,
            events,
            command_rx,
            manual_compaction,
            subscriptions,
        )
        .await;
        // The listener holds a clone of the sender; revoking it is what
        // lets the event channel close, so a driver reading to the end sees
        // the end. Dropping only the local handles would hang it forever.
        stream.dispose();
        added.dispose();
        removed.dispose();
        moved.dispose();
        let _ = done_tx.send(());
    });

    Ok(Driven {
        handle: AgentHandle {
            commands,
            events: event_rx,
            task,
        },
        done: done_rx,
        agent,
    })
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
        &["agents", "agent-loop"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // `approval` because the gate resolves the policy per call rather than
        // capturing it — the same reason the static approval row declares it.
        &[
            "tools",
            "llm",
            "compaction",
            "approval",
            "session-defaults",
            "grants",
        ]
    }
    fn provides(&self) -> &'static [&'static str] {
        // A driver renders prompts and answers them, so it fills the asking
        // seams too — and the standalone rows that would otherwise claim them
        // must stand down.
        &[
            "ui",
            "agent-handle",
            "user-questions",
            "approval",
            "tool-driver",
        ]
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

        let wire = wire();
        let asker = Arc::new(Asker::new(
            ctx.clone(),
            wire.events.clone(),
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

        let initial = wire.commands.clone();
        let Driven {
            handle,
            done,
            agent,
        } = spawn(
            ctx,
            wire,
            asker.clone(),
            crate::agent::CreateAgent::root(ctx),
        )
        .await?;
        let _ = ctx
            .provide::<crate::seams::ToolDriverSvc>(Arc::new(HandleToolDriver {
                session: agent.session_id().to_string(),
                asker,
            }))
            .map_err(|e| e.to_string())?;

        let front = Arc::new(HandleFrontEnd {
            handle: Mutex::new(Some(handle)),
            done: Mutex::new(Some(done)),
            initial: Mutex::new(Some(initial)),
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
        answering_a_nudge: false,
        usage_round: None,
        manual_compaction: Default::default(),
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
