//! The conversation, as an irreversible stream of blocks.
//!
//! One producer, six kinds of block. The awkward part is that a tool call is
//! **one block from two facts** — the call arrives with the assistant message
//! and the result arrives later, possibly out of order relative to its
//! siblings. Correlating them by `call_id` is what keeps the transcript one
//! row per call instead of two.

use crate::i18n::{t, Msg};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use atomcode_harness::seams::Question;
use atomcode_harness::session::{InjectionOrigin, SessionEvent};

use crate::block::{BlockId, Coord, StreamWriter};
use crate::content::{
    ChoiceBlock, InjectedBlock, ModelSaid, ModelThought, NoticeBlock, Outcome, ToolCallBlock,
    TurnEndBlock, TurnStats, UserSaid,
};
use crate::module::Producer;

pub const ID: &str = "transcript";

#[derive(Default)]
struct Open {
    /// The assistant text block currently accumulating chunks.
    text: Option<(BlockId, String)>,
    /// The reasoning block currently accumulating chunks.
    thought: Option<(BlockId, String)>,
    /// Tool calls waiting for their result, by `call_id`.
    calls: HashMap<String, (BlockId, ToolCallBlock)>,
    /// The question on the screen waiting for an answer, with the question
    /// itself: the answer says `allow`, and only the question can say what was
    /// being allowed. One at a time — a person answers one thing at a time.
    asked: Option<(BlockId, Question)>,
    /// What the turn in flight has cost so far, folded from its own `Usage` and
    /// `StepEnd` facts and handed to the block that closes it.
    ///
    /// Folded here, by the producer that owns the turn's blocks, rather than
    /// read off the status module: a producer folds its own facts, so live,
    /// replay and resume cannot disagree about what a turn cost — the same
    /// reason every other block is built here.
    stats: TurnStats,
    /// When the turn opened, on the log's own clock (`LoggedEvent::at`, in ms).
    /// The difference to the `TurnEnd` reading is the turn's duration — the log
    /// records no elapsed of its own, so it is folded from the two timestamps
    /// the same way the two front ends do it. `None` between turns.
    started_at: Option<u64>,
    /// The task list as the turns so far left it, so a turn that ends on its
    /// own can say whether it left work open. Fed the same facts the panel is.
    plan: crate::modules::todo::Plan,
    /// Whether the turn in flight made a call to the task list. A turn that
    /// never touched it — a question about something else, answered — is not
    /// stopping on the list's open items, which an earlier turn left there.
    touched_plan: bool,
    /// The last thing the turn in flight said, for whether it stopped on a
    /// question to the person.
    last_reply: String,
}

/// Turns session facts into what a person reads.
#[derive(Default)]
pub struct Transcript {
    open: Mutex<Open>,
    /// Which turn each `TurnStart` opened, by its sequence number: what an undo
    /// names the turn it went back to by (`docs/adr/0024` §17).
    turns: Mutex<Vec<(atomcode_harness::session::SeqNo, u64)>>,
    /// How many turns have ended cleanly, so far — the index into `DONE_LABELS`
    /// the next clean stop uses. Advanced only on a clean stop and only here, so
    /// the rotation is the same on live, replay and resume (it re-folds the same
    /// facts in the same order). Reset with the stream.
    done_seq: Mutex<usize>,
    /// What an undo of a stopped-before-answering turn is read against — see
    /// [`Unanswered`].
    unanswered: Mutex<Unanswered>,
}

/// Turns the person stopped before the model said anything, as far as the facts
/// so far tell.
///
/// Esc on such a turn hands the words back to the composer and takes the turn
/// back (`plugin::try_retract`). The undo draws nothing: the turn goes the way
/// every taken-back turn goes, and the `已回到第 N 轮之前` line an undo normally
/// leaves would be a trace of a message the person has in their hands again, in
/// a conversation that, as far as they are concerned, never had it. Read off
/// the log, so a resumed session draws the same thing.
#[derive(Default)]
struct Unanswered {
    /// Turns something of the model's reached the person in.
    answered: std::collections::HashSet<u64>,
    /// Turns the person stopped.
    stopped: std::collections::HashSet<u64>,
    /// The last turn anybody said anything in.
    last_said: Option<u64>,
}

impl Unanswered {
    fn absorb(&mut self, fact: &SessionEvent) {
        match fact {
            SessionEvent::UserMessage { turn, .. } => self.last_said = Some(*turn),
            SessionEvent::PartialReply { text, .. } if text.trim().is_empty() => {}
            SessionEvent::AssistantMessage { turn, .. }
            | SessionEvent::PartialReply { turn, .. }
            | SessionEvent::ToolStarted { turn, .. }
            | SessionEvent::Asked { turn, .. } => {
                self.answered.insert(*turn);
                self.last_said = Some(*turn);
            }
            SessionEvent::Interrupted { turn, .. } => {
                self.stopped.insert(*turn);
            }
            _ => {}
        }
    }

    /// Whether an undo back to before `turn` takes back only a turn the person
    /// stopped before anything came back — the last one anybody said anything in.
    fn only_a_stopped_prompt(&self, turn: u64) -> bool {
        self.last_said == Some(turn)
            && self.stopped.contains(&turn)
            && !self.answered.contains(&turn)
    }
}

/// The card a question draws, before or after it is answered.
///
/// One function for both, because both are the same card: an amendment has to
/// leave the question and the options exactly where they were, and two builders
/// that agree until one changes is how a card ends up rewriting itself when it
/// is answered.
fn card_for(question: &Question, answer: Option<String>) -> ChoiceBlock {
    ChoiceBlock {
        question: crate::ask::recorded(question),
        options: question
            .options
            .iter()
            .map(|a| crate::ask::answer_label(&a.value, &a.label))
            .collect(),
        answer,
    }
}

impl Transcript {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

/// What an injected block says about where it came from.
///
/// The kind, not the sender's id: a peer's message already names the sender
/// in its own text, and a session id on screen is an identifier nobody reads.
/// Debug-formatting the origin put `peer { from: "1789…/scout" }` in front of
/// every report, which is a struct dump, not a label.
fn origin_label(origin: &InjectionOrigin) -> String {
    match origin {
        // A sender this tree has never held is a job run elsewhere reporting
        // back. Calling it `peer` would put it in the same breath as a team
        // member, and the person reading the block has no way to tell the two
        // apart — the log has always been able to say which it was.
        InjectionOrigin::Peer { outside: true, .. } => t(Msg::InjectedFromBackground).into_owned(),
        InjectionOrigin::Peer { .. } => "peer".into(),
        InjectionOrigin::Memory => "memory".into(),
        InjectionOrigin::Reminder => "reminder".into(),
        InjectionOrigin::Continuation => "continuation".into(),
        InjectionOrigin::InternalNudge => "nudge".into(),
        InjectionOrigin::CompactionSummary => "compaction summary".into(),
        InjectionOrigin::PersonToMember { member } => format!("you → {member}"),
        InjectionOrigin::TeamNote { member } => format!("about {member}"),
    }
}

/// What an injected block is filed under, for the screen.
///
/// One kind per origin, and not the same string as the label above, because the
/// two answer to different readers. The label is read by a person; this is keyed
/// by [`Presentation`], which decides what opens on screen. Keeping them apart
/// is what lets `/showinject reminder` name a thing without also naming the text
/// that appears next to it — labels are prose, and prose is free to change.
///
/// [`Presentation`]: crate::host::Presentation
pub(crate) fn origin_kind(origin: &InjectionOrigin) -> &'static str {
    match origin {
        // A job run elsewhere reporting back is its own kind: it folds to the
        // one line that says it finished, which a teammate's note does not.
        InjectionOrigin::Peer { outside: true, .. } => "injected:background",
        InjectionOrigin::Peer { .. } => "injected:peer",
        InjectionOrigin::Memory => "injected:memory",
        InjectionOrigin::Reminder => "injected:reminder",
        InjectionOrigin::Continuation => "injected:continuation",
        InjectionOrigin::InternalNudge => "injected:nudge",
        InjectionOrigin::CompactionSummary => "injected:compaction",
        InjectionOrigin::PersonToMember { .. } => "injected:to-member",
        InjectionOrigin::TeamNote { .. } => "injected:team-note",
    }
}

impl Producer for Transcript {
    fn id(&self) -> &'static str {
        ID
    }

    fn reset(&self) {
        *self.open.lock().expect("transcript poisoned") = Open::default();
        self.turns.lock().expect("transcript poisoned").clear();
        *self.done_seq.lock().expect("transcript poisoned") = 0;
        *self.unanswered.lock().expect("transcript poisoned") = Unanswered::default();
    }

    fn absorb(&self, logged: &atomcode_harness::session::LoggedEvent, out: &mut StreamWriter<'_>) {
        let fact = &logged.event;
        let mut open = self.open.lock().expect("transcript poisoned");
        let at = Coord::new(fact.turn(), 0);
        open.plan.absorb(fact);
        // Read before this fact is folded in, so a `Rewound` is judged by the
        // turns before it.
        let silent_undo = match fact {
            SessionEvent::Rewound { to, scope, .. } if scope.takes_back_conversation() => self
                .turns
                .lock()
                .expect("transcript poisoned")
                .iter()
                .find(|(seq, _)| seq == to)
                .is_some_and(|(_, turn)| {
                    self.unanswered
                        .lock()
                        .expect("transcript poisoned")
                        .only_a_stopped_prompt(*turn)
                }),
            _ => false,
        };
        self.unanswered
            .lock()
            .expect("transcript poisoned")
            .absorb(fact);
        match fact {
            // Where the turns start, so an undo can say which one it went back
            // to; and the undo itself, as a line in the stream that says so.
            SessionEvent::TurnStart { turn } => {
                self.turns
                    .lock()
                    .expect("transcript poisoned")
                    .push((logged.seq, *turn));
                // Where the turn's clock starts, for the duration its summary
                // reports. Read off the fact rather than a clock, so replay is
                // deterministic.
                open.started_at = Some(logged.at);
                open.touched_plan = false;
                open.last_reply.clear();
            }
            SessionEvent::Rewound { to, scope, .. } => {
                let to_turn = self
                    .turns
                    .lock()
                    .expect("transcript poisoned")
                    .iter()
                    .find(|(seq, _)| *seq == *to)
                    .map(|(_, turn)| *turn);
                // The turns taken back take their part of the plan with them,
                // as the panel's projection does.
                if let Some(turn) = to_turn.filter(|_| scope.takes_back_conversation()) {
                    open.plan.retract_from(turn);
                }
                if silent_undo {
                    return;
                }
                out.emit(
                    at,
                    Arc::new(crate::content::RewoundBlock {
                        to_turn,
                        scope: *scope,
                    }),
                );
            }
            SessionEvent::UserMessage { text, .. } => {
                // A picture the runtime captioned for a text-only model folds the
                // whole recognition into the message so the model reads it. Split
                // it back out: the person's words stay a user line, the VL
                // recognition becomes its own block, folded to one row and opened
                // on a click — not a wall of text under every screenshot.
                if let Some((said, model, caption)) = crate::content::split_vl_caption(text) {
                    if !said.is_empty() {
                        out.emit(at, Arc::new(UserSaid(said)));
                    }
                    out.emit(
                        at,
                        Arc::new(crate::content::VlCaptionBlock {
                            model,
                            text: caption,
                        }),
                    );
                } else {
                    out.emit(at, Arc::new(UserSaid(text.clone())));
                }
            }

            // Chunks accumulate into one live block rather than one block per
            // token: the model said one thing, and the transcript should show
            // one thing being said.
            SessionEvent::AssistantChunk {
                delta,
                reasoning,
                turn,
                round,
                ..
            } => {
                let at = Coord::new(*turn, *round);
                let slot = if *reasoning {
                    &mut open.thought
                } else {
                    &mut open.text
                };
                match slot {
                    Some((id, acc)) => {
                        acc.push_str(delta);
                        let content: Arc<dyn crate::block::Content> = if *reasoning {
                            Arc::new(ModelThought(acc.clone()))
                        } else {
                            Arc::new(ModelSaid(acc.clone()))
                        };
                        out.amend(*id, content);
                    }
                    None => {
                        let acc = delta.clone();
                        let content: Arc<dyn crate::block::Content> = if *reasoning {
                            Arc::new(ModelThought(acc.clone()))
                        } else {
                            Arc::new(ModelSaid(acc.clone()))
                        };
                        *slot = Some((out.open(at, content), acc));
                    }
                }
            }

            // The message settles whatever was streaming and opens a block per
            // call. Calls are opened *here*, before they run, so a person sees
            // what was asked for in the order it was asked for.
            SessionEvent::AssistantMessage {
                text,
                reasoning,
                tool_calls,
                turn,
                round,
                ..
            } => {
                let at = Coord::new(*turn, *round);
                if tool_calls
                    .iter()
                    .any(|c| atomcode_capabilities::tools::todo::is_todo_call(&c.name))
                {
                    open.touched_plan = true;
                }
                if !text.trim().is_empty() {
                    open.last_reply = text.clone();
                }
                match open.thought.take() {
                    Some((id, _)) => {
                        out.settle(id);
                    }
                    None if !reasoning.is_empty() => {
                        out.emit(at, Arc::new(ModelThought(reasoning.clone())));
                    }
                    None => {}
                }
                match open.text.take() {
                    Some((id, _)) => {
                        out.settle(id);
                    }
                    // No chunks arrived (a non-streaming provider, or a
                    // recovered partial): the finished text is still a fact.
                    None if !text.is_empty() => {
                        out.emit(at, Arc::new(ModelSaid(text.clone())));
                    }
                    None => {}
                }
                for call in tool_calls {
                    let mut block = ToolCallBlock::pending(&call.id, &call.name, &call.arguments);
                    if let Some(dir) = out.dir() {
                        block = block.ran_in(dir);
                    }
                    let id = out.open(at, Arc::new(block.with(Outcome::Pending)));
                    open.calls.insert(call.id.clone(), (id, block));
                }
            }

            // What a stopped reply had said. Live, the chunks already drew it
            // and it only settles; replayed from a log that keeps no chunks, it
            // is the only record of the words the person read.
            SessionEvent::PartialReply {
                text,
                reasoning,
                turn,
                round,
            } => {
                let at = Coord::new(*turn, *round);
                match open.thought.take() {
                    Some((id, _)) => {
                        out.settle(id);
                    }
                    None if !reasoning.is_empty() => {
                        out.emit(at, Arc::new(ModelThought(reasoning.clone())));
                    }
                    None => {}
                }
                match open.text.take() {
                    Some((id, _)) => {
                        out.settle(id);
                    }
                    None if !text.is_empty() => {
                        out.emit(at, Arc::new(ModelSaid(text.clone())));
                    }
                    None => {}
                }
            }

            SessionEvent::ToolResultLogged {
                call_id,
                content,
                is_error,
                ..
            } => {
                let named = open
                    .calls
                    .get(call_id)
                    .filter(|_| !*is_error)
                    .and_then(|(_, block)| named_todo_update(block, call_id, content, &open.plan));
                let outcome = if *is_error {
                    Outcome::Failed(content.clone())
                } else {
                    Outcome::Ok(named.unwrap_or_else(|| content.clone()))
                };
                match open.calls.remove(call_id) {
                    Some((id, block)) => {
                        out.amend(id, Arc::new(block.with(outcome)));
                        out.settle(id);
                    }
                    // A result with no call: the log is out of shape, but a
                    // person should still see what came back.
                    None => {
                        let orphan = ToolCallBlock::pending(call_id, "(unpaired)", "{}");
                        out.emit(at, Arc::new(orphan.with(outcome)));
                    }
                }
            }

            SessionEvent::Notice {
                notice,
                detail,
                retry,
                ..
            } => {
                // The runtime writes its rate-limit wait as an English sentence
                // (`host_rows::RateLimitCoding`); said here in the person's
                // language, as the classic screen says it. A provider retry
                // carries its numbers apart from the provider's error, so it is
                // said the same way. Anything else — and a retry logged before
                // the numbers were kept — is shown as it was written.
                let detail = match (notice, retry) {
                    (atomcode_harness::session::NoticeKind::RateLimited, _) => {
                        rate_limit_wait(detail).unwrap_or_else(|| detail.clone())
                    }
                    (atomcode_harness::session::NoticeKind::ProviderRetry, Some(retry)) => {
                        t(Msg::TranscriptProviderRetry {
                            reason: detail,
                            seconds: retry.backoff_secs,
                            attempt: retry.attempt,
                            max: retry.max_attempts,
                        })
                        .into_owned()
                    }
                    _ => detail.clone(),
                };
                out.emit(at, Arc::new(NoticeBlock { detail }));
            }

            // ---- things the harness did that change what the model sees -----
            //
            // All four are what `NoticeBlock` is for — "something the harness
            // did that a person should know and the model must not" — and all
            // four were in the log and off the screen
            // (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md`
            // A5, A6, A8, A9). A person reading a conversation where the middle
            // silently left, or where a tool's output is not what the model was
            // handed, needs it said.
            SessionEvent::Compacted { through, .. } => {
                out.emit(
                    at,
                    Arc::new(NoticeBlock {
                        detail: t(Msg::TranscriptCompacted { through: *through }).into_owned(),
                    }),
                );
            }
            SessionEvent::MessagesRewritten { texts, .. } => {
                out.emit(
                    at,
                    Arc::new(NoticeBlock {
                        detail: t(Msg::TranscriptShortened { count: texts.len() }).into_owned(),
                    }),
                );
            }
            SessionEvent::ToolResultsStubbed { through, .. } => {
                out.emit(
                    at,
                    Arc::new(NoticeBlock {
                        detail: t(Msg::TranscriptDropped { through: *through }).into_owned(),
                    }),
                );
            }
            SessionEvent::RateLimitPaused { pause, .. } => {
                out.emit(
                    at,
                    Arc::new(NoticeBlock {
                        detail: rate_limit_pause(pause),
                    }),
                );
            }
            // A member of this session's team stopped for good
            // (`docs/adr/0024` §13). The team panel says so while it is
            // mounted; the conversation should say it too, because a lead whose
            // member is gone is reading a conversation that will not continue.
            SessionEvent::Stopped { .. } => {
                out.emit(
                    at,
                    Arc::new(NoticeBlock {
                        detail: t(Msg::TranscriptMemberEnded).into_owned(),
                    }),
                );
            }

            SessionEvent::Injected { text, origin, .. } => {
                // A member's report is drawn the way a background result is: a
                // head that says who came back, and the report under it, folded
                // until a click (`Presentation::default_folds`). Raw, it was the
                // whole report — often a hundred lines with its own headings —
                // standing open in the lead's conversation with no lid.
                let (text, result) = match origin {
                    InjectionOrigin::Peer { outside: true, .. } => (text.clone(), true),
                    InjectionOrigin::Peer { from, .. } => {
                        let name = from.rsplit('/').next().unwrap_or(from);
                        let body = crate::content::member_report_body(name, text);
                        // The head keeps what the team's own turn-end line said
                        // that matters to a person: a member that was cancelled
                        // or broke off did not report, and must not read as if
                        // it had.
                        let head = match crate::content::member_turn_end(name, text) {
                            None | Some("Stopped") => t(Msg::MemberReportedBack { name }),
                            Some("Cancelled") => t(Msg::MemberTurnCancelled { name }),
                            Some(why) => t(Msg::MemberTurnEndedEarly { name, why }),
                        };
                        (format!("{head}\n{body}"), true)
                    }
                    _ => (text.clone(), false),
                };
                out.emit(
                    at,
                    Arc::new(InjectedBlock {
                        kind: origin_kind(origin),
                        origin: origin_label(origin),
                        text,
                        result,
                    }),
                );
            }

            // A question was put. Opened rather than settled, because it has no
            // answer yet — the same shape as a tool call, and for the same
            // reason: the card is what the person is deciding about, and it is
            // already in the conversation rather than only in a panel that
            // would have to be mounted to show it.
            SessionEvent::Asked { question, .. } => {
                let id = out.open(at, Arc::new(card_for(question, None)));
                open.asked = Some((id, question.clone()));
            }

            // And it was closed — with an answer or without one. Amended in
            // place, so the words the person was deciding between stay where
            // they were and only the answer is filled in.
            SessionEvent::Answered { answer, by, .. } => {
                let by_mode = by == atomcode_harness::seams::ANSWERED_BY_MODE;
                let said = match (answer, &open.asked) {
                    // The label the asker gave the value it sent back, so the
                    // word on the card is the word the card offered.
                    (Some(value), Some((_, asked))) => crate::ask::answer_label(
                        value,
                        &asked
                            .options
                            .iter()
                            .find(|a| &a.value == value)
                            .map(|a| a.label.clone())
                            .unwrap_or_else(|| value.clone()),
                    ),
                    (Some(value), None) => value.clone(),
                    // No answer is a refusal. Written the same way whether a
                    // person declined or the turn was cancelled out from under
                    // the question: both are "nobody said yes".
                    //
                    // `by` is deliberately not drawn. It is in the log because a
                    // record of who allowed what is worth having once two
                    // clients can answer; a card that read "拒绝 · the person at
                    // the terminal" would be the log leaking onto the screen.
                    (None, _) => {
                        crate::ask::answer_label(atomcode_harness::seams::ANSWER_DENY, "declined")
                    }
                };
                match open.asked.take() {
                    Some((id, asked)) if by_mode => {
                        let card = card_for(&asked, Some(said));
                        out.amend(id, Arc::new(crate::content::AllowedByMode(card)));
                        out.settle(id);
                    }
                    Some((id, asked)) => {
                        out.amend(id, Arc::new(card_for(&asked, Some(said))));
                        out.settle(id);
                    }
                    // Nothing was asked on screen and nobody answered: nothing
                    // to draw.
                    None if by_mode => {}
                    // An answer with no question in front of it: the log is out
                    // of shape, but a person should still see what was decided.
                    None => {
                        out.emit(
                            at,
                            Arc::new(ChoiceBlock {
                                question: "(unpaired)".into(),
                                options: Vec::new(),
                                answer: Some(said),
                            }),
                        );
                    }
                }
            }

            SessionEvent::TurnEnd { stop, error, .. } => {
                // Anything still open never got its answer. Saying so is the
                // honest ending: a call that was cut is not a call that failed.
                if let Some((id, _)) = open.text.take() {
                    out.settle(id);
                }
                if let Some((id, _)) = open.thought.take() {
                    out.settle(id);
                }
                // A question the turn ended on top of is a refusal by the seam's
                // own contract — every caller reads a missing answer as one —
                // so the card settles saying that rather than staying lit as
                // though someone were still deciding.
                if let Some((id, asked)) = open.asked.take() {
                    out.amend(
                        id,
                        Arc::new(card_for(
                            &asked,
                            Some(crate::ask::answer_label(
                                atomcode_harness::seams::ANSWER_DENY,
                                "declined",
                            )),
                        )),
                    );
                    out.settle(id);
                }
                for (_, (id, block)) in open.calls.drain() {
                    out.amend(id, Arc::new(block.with(Outcome::Interrupted)));
                    out.settle(id);
                }
                // The turn's duration: the gap between this reading and the one
                // stamped at `TurnStart`, both on the log's own clock. A turn
                // with no recorded start (a log that predates the stamp) reports
                // none rather than a nonsense figure.
                let start = open.started_at.take();
                // Taken, not read: a turn's cost is spent when the turn ends,
                // and the next `TurnStart` would otherwise be the only thing
                // standing between one turn's figures and the next turn's line.
                let mut stats = std::mem::take(&mut open.stats);
                stats.elapsed_ms = start.map(|s| logged.at.saturating_sub(s)).unwrap_or(0);
                // Asked only of a turn the model ended on its own (every other
                // stop already says it was cut short), that worked the list this
                // turn, and that did not end on a question to the person — the
                // same conditions the runtime nudges under, so the line never
                // tells someone to send "继续" when they owe an answer, or on a
                // turn about something else.
                let open_items = if matches!(stop, atomcode_harness::seams::StopReason::Stopped)
                    && open.touched_plan
                    && !atomcode_capabilities::tools::todo::ends_on_a_question(&open.last_reply)
                {
                    open.plan.open_items()
                } else {
                    0
                };
                // A clean stop takes the next rotation slot and advances it; every
                // other outcome leaves the rotation where it is (its label is
                // ignored) so the celebratory verbs are not burned on failures —
                // nor on a stop that left the list open.
                let done_index = {
                    let mut seq = self.done_seq.lock().expect("transcript poisoned");
                    let idx = *seq;
                    if matches!(stop, atomcode_harness::seams::StopReason::Stopped)
                        && open_items == 0
                    {
                        *seq += 1;
                    }
                    idx
                };
                out.emit(
                    at,
                    Arc::new(TurnEndBlock {
                        // The typed reason, not a rendering of it: how the turn
                        // ended is what the block draws, and a `{:?}` here would
                        // put a Rust identifier on the screen (see
                        // `content::turn_end_note`).
                        stop: *stop,
                        error: error.clone(),
                        stats,
                        done_index,
                        open_items,
                        ended_at: crate::content::clock_of(logged.at),
                    }),
                );
            }

            // What the turn cost, in the two facts that carry it. `step` and
            // `round` are one counter in the loop, so the steps are read off
            // the same number the requests are numbered with.
            SessionEvent::StepEnd {
                step, tool_calls, ..
            } => {
                open.stats.steps = *step;
                // Each step reports the calls it ran; the turn's tool count is
                // their sum. Unlike `steps` (a running counter read verbatim),
                // this one adds up across the turn.
                open.stats.tools = open.stats.tools.saturating_add(*tool_calls);
            }

            // A round's usage, merged by the loop into one figure per round.
            SessionEvent::Usage { usage, .. } => {
                // Summed, both: the line closing the turn says what it cost and
                // how much of that the cache served, and every request is billed
                // in full — the prefix it re-sends is cheap only when it hits.
                // The last reading alone said `99% cached` for a turn that missed
                // the cache outright twice (see `TurnStats`).
                open.stats.prompt += u64::from(usage.prompt);
                open.stats.cached += u64::from(usage.cached);
                // Output is the one figure that does add up: each round
                // generated its own, and the loop has already folded whatever
                // the provider re-sent within a round.
                open.stats.completion += u64::from(usage.completion);
            }

            // Turn and step boundaries are coordinates, not blocks; request
            // headers are the status module's business.
            _ => {}
        }
    }
}

/// The runtime's rate-limit wait (`rate limited; retrying in 95s`), in the
/// person's language. `None` for any other wording, which is then shown as it
/// was written rather than guessed at.
fn rate_limit_wait(detail: &str) -> Option<String> {
    let secs = detail
        .strip_prefix("rate limited; retrying in ")?
        .strip_suffix('s')?
        .parse::<u64>()
        .ok()?;
    Some(t(Msg::TranscriptRateLimitWaiting { secs }).into_owned())
}

/// An incremental todo update's result with the task's name in it.
///
/// The tool answers `#5 → completed`: the number is all the call carried, and on
/// its own it says nothing — "#5" is only meaningful beside the list, and the
/// list is a panel that may be gone by the time anyone reads the row. The plan
/// this transcript already keeps knows what #5 is, so the row becomes
/// `#5 <task> → completed`, the way the classic screen draws it
/// (`enrich_todo_detail`). `None` for anything else, and for a number the plan
/// has no task for — the tool's own words stand then.
fn named_todo_update(
    block: &ToolCallBlock,
    call_id: &str,
    result: &str,
    plan: &crate::modules::todo::Plan,
) -> Option<String> {
    if !matches!(block.name.as_str(), "todo" | "todowrite") {
        return None;
    }
    let args: serde_json::Value = serde_json::from_str(&block.args).ok()?;
    if args.get("action").and_then(|a| a.as_str()) != Some("update") {
        return None;
    }
    let id = args.get("id").and_then(|x| x.as_u64())?;
    let head = format!("#{id}");
    let rest = result.trim().strip_prefix(&head)?;
    let title = plan.title_as_of(call_id, id)?;
    let title = if crate::width::str_width(&title) > TODO_TITLE_CELLS {
        format!(
            "{}…",
            crate::width::take_width(&title, TODO_TITLE_CELLS.saturating_sub(1))
        )
    } else {
        title
    };
    Some(format!("{head} {title}{rest}"))
}

/// The most of a task's name an update row carries: the row is a line in the
/// transcript, and the full name is in the plan.
const TODO_TITLE_CELLS: usize = 80;

/// Why a rate-limited turn stopped, as the classic screen says it.
///
/// A CodingPlan window carries window data (a reset time and/or a label); a 429
/// from anywhere else carries neither and must not be dressed up as the plan's
/// quota — it gets the provider's own reason instead. Either way the time left
/// is said when known, and never a dangling "until" with nothing after it.
fn rate_limit_pause(pause: &atomcode_harness::events::RateLimitPause) -> String {
    let left = pause.secs_until_reset.map(wait_left);
    let plan = !pause.reset_at_display.is_empty() || !pause.reset_label.is_empty();
    if plan {
        let mut said = t(Msg::TranscriptWindowExhausted {
            until: &pause.reset_at_display,
            left: left.as_deref(),
        })
        .into_owned();
        // What the other end said about it, when it said anything: kept from
        // before, though the classic screen leaves it out for a plan window.
        if let Some(message) = pause.server_message.as_deref().map(str::trim) {
            if !message.is_empty() {
                said.push_str(&format!(" · {message}"));
            }
        }
        said
    } else {
        t(Msg::TranscriptRateLimitedElsewhere {
            reason: pause
                .server_message
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty()),
            left: left.as_deref(),
        })
        .into_owned()
    }
}

/// How long until a window reopens, the way the classic screen writes it:
/// `2h11m`, `45m`, `30s`.
fn wait_left(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime's English wait sentence is said in the person's language;
    /// any other wording is left alone rather than guessed at.
    #[test]
    fn a_rate_limit_wait_is_said_in_the_persons_language() {
        assert_eq!(
            rate_limit_wait("rate limited; retrying in 95s"),
            Some(t(Msg::TranscriptRateLimitWaiting { secs: 95 }).into_owned())
        );
        assert_eq!(rate_limit_wait("something else happened"), None);
        assert_eq!(rate_limit_wait("rate limited; retrying in soon"), None);
    }

    /// A plan window says when it reopens and how long is left; a 429 from
    /// anywhere else says the provider's reason and is never dressed up as the
    /// plan's quota; neither leaves a dangling "until" when the time is missing.
    #[test]
    fn a_rate_limit_pause_says_what_it_is_and_how_long() {
        use atomcode_harness::events::RateLimitPause;
        let plan = RateLimitPause {
            reset_at_display: "18:09".into(),
            reset_label: "5h".into(),
            secs_until_reset: Some(2 * 3600 + 11 * 60),
            server_message: None,
        };
        assert_eq!(
            rate_limit_pause(&plan),
            t(Msg::TranscriptWindowExhausted {
                until: "18:09",
                left: Some("2h11m")
            })
            .into_owned()
        );

        let plan_no_time = RateLimitPause {
            reset_label: "5h".into(),
            ..RateLimitPause::default()
        };
        assert_eq!(
            rate_limit_pause(&plan_no_time),
            t(Msg::TranscriptWindowExhausted {
                until: "",
                left: None
            })
            .into_owned()
        );

        let elsewhere = RateLimitPause {
            secs_until_reset: Some(90),
            server_message: Some("  余额不足，请充值  ".into()),
            ..RateLimitPause::default()
        };
        assert_eq!(
            rate_limit_pause(&elsewhere),
            t(Msg::TranscriptRateLimitedElsewhere {
                reason: Some("余额不足，请充值"),
                left: Some("1m")
            })
            .into_owned()
        );

        let bare = rate_limit_pause(&RateLimitPause::default());
        assert_eq!(
            bare,
            t(Msg::TranscriptRateLimitedElsewhere {
                reason: None,
                left: None
            })
            .into_owned()
        );
        assert!(
            !bare.trim_end().ends_with("等到") && !bare.trim_end().ends_with("until"),
            "{bare}"
        );
    }

    #[test]
    fn the_time_left_reads_as_the_classic_screen_writes_it() {
        assert_eq!(wait_left(2 * 3600 + 11 * 60 + 5), "2h11m");
        assert_eq!(wait_left(45 * 60), "45m");
        assert_eq!(wait_left(30), "30s");
    }
    use crate::block::{Slot, Stream};
    use crate::conformance;

    fn fold(facts: &[SessionEvent]) -> Stream {
        let mut s = Stream::new();
        let t = Transcript::default();
        for (i, f) in facts.iter().enumerate() {
            let mut w = s.writer(ID);
            t.absorb(&conformance::logged(i, f), &mut w);
        }
        s
    }

    /// Every line of every block, joined — for judging that something was said
    /// at all, rather than where.
    fn said(s: &Stream) -> String {
        s.slots()
            .iter()
            .flat_map(|x| {
                crate::block::Content::lines(
                    &*x.block().content,
                    &crate::block::RenderCtx::bare(80),
                )
            })
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What the harness did to what the model sees is on the screen too
    /// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A5, A6,
    /// A9): a conversation whose middle was compacted away, whose tool output
    /// the model was handed shorter than it is here, or whose results stopped
    /// being sent, reads as a conversation that makes no sense — unless it says
    /// so.
    #[test]
    fn what_the_model_was_handed_instead_is_said_on_screen() {
        use atomcode_harness::session::RewrittenText;
        let s = fold(&[
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::Compacted {
                turn: 1,
                through: 7,
                from: 1,
                summary: "早先说过的事".into(),
            },
            SessionEvent::MessagesRewritten {
                turn: 1,
                texts: vec![
                    RewrittenText {
                        seq: 3,
                        text: "…".into(),
                    },
                    RewrittenText {
                        seq: 5,
                        text: "…".into(),
                    },
                ],
            },
            SessionEvent::ToolResultsStubbed {
                turn: 1,
                through: 9,
            },
        ]);
        let drawn = said(&s);
        assert!(
            drawn.contains("压成一段摘要") && drawn.contains("#7"),
            "the compaction says how far it reached:\n{drawn}"
        );
        assert!(
            drawn.contains("2 处工具输出被就地换短"),
            "and how much the model was handed differently:\n{drawn}"
        );
        assert!(
            drawn.contains("#9") && drawn.contains("没有再发给模型"),
            "and where results stopped being sent:\n{drawn}"
        );
    }

    /// A retry is said once, in the person's language, around the provider's
    /// own error; a retry logged before its numbers were kept apart is shown as
    /// it was written.
    #[test]
    fn a_provider_retry_says_the_error_and_when_it_tries_again() {
        use atomcode_harness::session::{NoticeKind, RetryAttempt};
        let s = fold(&[
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::Notice {
                turn: 1,
                notice: NoticeKind::ProviderRetry,
                detail: "connection refused".into(),
                retry: Some(RetryAttempt {
                    attempt: 1,
                    max_attempts: 2,
                    backoff_secs: 3,
                }),
            },
            SessionEvent::Notice {
                turn: 1,
                notice: NoticeKind::ProviderRetry,
                detail: "gateway reset; retrying in 6s (2/2)".into(),
                retry: None,
            },
        ]);
        let drawn = said(&s);
        let said_once = t(Msg::TranscriptProviderRetry {
            reason: "connection refused",
            seconds: 3,
            attempt: 1,
            max: 2,
        })
        .into_owned();
        assert!(drawn.contains(&said_once), "{drawn}");
        assert!(
            drawn.contains("gateway reset; retrying in 6s (2/2)"),
            "{drawn}"
        );
    }

    /// A pause is on screen with the time it ends, because the screen is
    /// otherwise a conversation that simply stopped (A6).
    #[test]
    fn being_rate_limited_says_so_and_says_until_when() {
        use atomcode_harness::session::RateLimitPause;
        let s = fold(&[
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::RateLimitPaused {
                turn: 1,
                pause: RateLimitPause {
                    reset_at_display: "14:30".into(),
                    reset_label: "14:30".into(),
                    secs_until_reset: Some(600),
                    server_message: Some("配额用完了".into()),
                },
            },
        ]);
        let drawn = said(&s);
        let window = t(Msg::TranscriptWindowExhausted {
            until: "14:30",
            left: Some("10m"),
        })
        .into_owned();
        // The head of the sentence — what happened, until when, how long is
        // left — fits one row; the rest wraps.
        let head = window.split(" · ").next().expect("a head");
        assert!(
            drawn.contains(head),
            "it says what happened, until when and how long is left:\n{drawn}"
        );
        assert!(
            drawn.contains("配额用完了"),
            "and what the other end said about it:\n{drawn}"
        );
    }

    /// A member that stopped for good says so in its own conversation (A8): the
    /// team panel is a panel a screen may not have mounted, and the log is what
    /// every screen reads.
    #[test]
    fn a_member_that_stopped_says_so_in_its_own_conversation() {
        let s = fold(&[
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::Stopped { turn: 1 },
        ]);
        assert!(
            said(&s).contains("已经结束"),
            "the stop is on screen:\n{}",
            said(&s)
        );
    }

    fn kinds(s: &Stream) -> Vec<&'static str> {
        s.slots().iter().map(|x| x.block().kind()).collect()
    }

    /// Esc before the model said anything takes the turn back, and the undo that
    /// does it leaves no line: the person has the words in the composer again,
    /// and a `已回到第 N 轮之前` for a message nobody answered is a trace of
    /// something that, for them, was never sent. An undo of a turn the model
    /// did answer still says so.
    #[test]
    fn taking_back_a_prompt_stopped_before_any_answer_leaves_no_line() {
        use atomcode_harness::session::RewindScope;
        let said = |turn: u64, text: &str| SessionEvent::UserMessage {
            turn,
            text: text.into(),
            images: Vec::new(),
        };
        let ended = |turn: u64| SessionEvent::TurnEnd {
            turn,
            stop: atomcode_kernel::event::StopReason::Cancelled,
            error: None,
        };
        let undo = SessionEvent::Rewound {
            turn: 2,
            to: 3,
            scope: RewindScope::Conversation,
        };
        let stopped = [
            SessionEvent::TurnStart { turn: 1 },
            said(1, "the first thing"),
            SessionEvent::TurnStart { turn: 2 },
            said(2, "你好"),
            SessionEvent::Interrupted {
                turn: 2,
                undone: false,
            },
            ended(2),
            undo.clone(),
        ];
        assert!(
            !kinds(&fold(&stopped)).contains(&"rewound"),
            "{:?}",
            kinds(&fold(&stopped))
        );

        let answered = [
            SessionEvent::TurnStart { turn: 1 },
            said(1, "the first thing"),
            SessionEvent::TurnStart { turn: 2 },
            said(2, "你好"),
            SessionEvent::PartialReply {
                turn: 2,
                round: 1,
                text: "你好!".into(),
                reasoning: String::new(),
            },
            SessionEvent::Interrupted {
                turn: 2,
                undone: false,
            },
            ended(2),
            undo,
        ];
        assert!(kinds(&fold(&answered)).contains(&"rewound"));
    }

    /// An undo is a line in the conversation, and it says which turn the session
    /// went back to and how far the undo reached (`docs/adr/0024` §17).
    ///
    /// The turn number is the transcript's to work out: the fact names the
    /// sequence number it rewound to, because that is what the log can be
    /// truncated by — and a person reads turns, not sequence numbers.
    #[test]
    fn an_undo_says_which_turn_it_went_back_to_and_how_far_it_reached() {
        use atomcode_harness::session::RewindScope;
        let said = |turn: u64, text: &str| SessionEvent::UserMessage {
            turn,
            text: text.into(),
            images: Vec::new(),
        };
        // Turn 2 opens at seq 3 — `conformance::logged` numbers from one.
        let facts = [
            SessionEvent::TurnStart { turn: 1 },
            said(1, "the first thing"),
            SessionEvent::TurnStart { turn: 2 },
            said(2, "the second thing"),
            SessionEvent::Rewound {
                turn: 2,
                to: 3,
                scope: RewindScope::Both,
            },
        ];
        let s = fold(&facts);
        let line = |s: &Stream| -> String {
            s.slots()
                .iter()
                .filter(|x| x.block().kind() == "rewound")
                .map(|x| {
                    crate::block::Content::lines(
                        &*x.block().content,
                        &crate::block::RenderCtx::bare(60),
                    )
                    .iter()
                    .map(|l| l.plain())
                    .collect::<Vec<_>>()
                    .join("\n")
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let drawn = line(&s);
        assert!(
            drawn.contains("第 2 轮"),
            "the turn it went back to, not the sequence number it was logged \
             against:\n{drawn}"
        );
        assert!(
            drawn.contains("对话与工作区"),
            "and how far it reached:\n{drawn}"
        );

        // A `to` that names no turn start — an undo whose turn the screen never
        // saw, which is what a resumed session can hand it — still draws a line.
        // Losing the undo entirely would leave a conversation that silently
        // disagrees with the model's.
        let mut earlier = facts.to_vec();
        earlier[4] = SessionEvent::Rewound {
            turn: 2,
            to: 99,
            scope: RewindScope::Conversation,
        };
        let drawn = line(&fold(&earlier));
        assert!(
            drawn.contains("更早的一轮") && drawn.contains("对话"),
            "an undo the screen cannot date is still an undo:\n{drawn}"
        );
    }

    #[test]
    fn an_answered_question_is_one_settled_card_in_the_conversation() {
        let s = fold(&conformance::facts());
        // The whole card, not just its summary line: what was asked, the answers
        // that were offered, and which one was taken. A card that lost its
        // options would still read as an answer — with no way to tell what the
        // person was choosing between.
        let cards: Vec<String> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "choice")
            .map(|x| {
                assert!(
                    x.is_settled(),
                    "a card is settled once it is closed: nothing about it is \
                     still being decided"
                );
                crate::block::Content::lines(
                    &*x.block().content,
                    &crate::block::RenderCtx::bare(80),
                )
                .iter()
                .map(|l| l.plain())
                .collect::<Vec<_>>()
                .join("\n")
            })
            .collect();
        // Three ways a question ends, and all three are on the screen: answered,
        // closed with nothing, and the turn ending on top of one. The third is
        // the one a fold can get wrong without anyone noticing — it arrives
        // with no answer of its own.
        assert_eq!(cards.len(), 3, "one card per question: {cards:?}");
        // What was asked, with the call's own subject. The screen used to compose
        // this itself, out of the question it happened to be holding — which is
        // exactly why a remount lost it.
        assert!(
            cards[0].contains("write_file") && cards[0].contains("notes.md"),
            "the call under review is on the card:\n{}",
            cards[0]
        );
        assert!(
            cards[0].contains("允许一次"),
            "and the answer that was taken, in the words the card offered it in:\n{}",
            cards[0]
        );
        // Closed with no answer: a refusal, not silence and not consent.
        assert!(
            cards[1].contains("拒绝") && cards[1].contains("Allow `bash` to run?"),
            "a question closed with nothing still says what it was:\n{}",
            cards[1]
        );
        // And the one the turn ended on top of. It never got an answer of its
        // own, and the seam's contract is that a missing answer is a refusal —
        // so the card must not keep saying the person has not decided yet.
        assert!(
            cards[2].contains("拒绝") && cards[2].contains("telemetry"),
            "a question a finished turn left behind settles as a refusal:\n{}",
            cards[2]
        );
    }

    crate::tui_conformance!(producer || Transcript::new() as Arc<dyn Producer>, as transcript_conformance);

    #[test]
    fn chunks_become_one_growing_block_not_one_block_each() {
        let s = fold(&conformance::facts()[..7]);
        let text: Vec<_> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "assistant")
            .collect();
        assert_eq!(text.len(), 1, "one thing said, one block");
        assert!(text[0].is_live(), "still being said");
        assert_eq!(
            text[0]
                .block()
                .content
                .lines(&crate::block::RenderCtx::bare(80))[0]
                .plain(),
            "Looking."
        );
    }

    #[test]
    fn a_tool_call_is_one_block_that_later_learns_its_result() {
        let s = fold(&conformance::facts());
        let calls: Vec<_> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "tool_call")
            .collect();
        // One block per call, not one per fact: the call and its result are
        // two facts that have to land in the same block. The corpus has three
        // calls, so six facts must fold into three blocks.
        let in_corpus = conformance::facts()
            .iter()
            .filter_map(|f| match f {
                atomcode_harness::session::SessionEvent::AssistantMessage {
                    tool_calls, ..
                } => Some(tool_calls.len()),
                _ => None,
            })
            .sum::<usize>();
        assert_eq!(
            calls.len(),
            in_corpus,
            "one block per call, not one per fact"
        );
        assert!(calls.iter().all(|c| c.is_settled()));
        // The outcome is in the words on the result line, not in the header
        // glyph — the header is `●` for every call, the way tuix draws it. That
        // is also a better thing to assert: it does not depend on colour, and a
        // person reading a colourless terminal is reading the same words.
        let rendered: Vec<Vec<String>> = calls
            .iter()
            .map(|c| {
                c.block()
                    .content
                    .lines(&crate::block::RenderCtx::bare(60))
                    .iter()
                    .map(|l| l.plain())
                    .collect()
            })
            .collect();
        assert!(rendered[0][0].contains("a.rs"), "{rendered:?}");
        assert!(!rendered[0][1].contains("失败"), "{rendered:?}");
        assert!(
            rendered[1][1].contains("失败"),
            "the failing one says so: {rendered:?}"
        );
    }

    #[test]
    fn a_turn_that_reported_usage_closes_with_what_it_cost() {
        // The corpus' turn 1: one step, two tool calls, one request reporting
        // 1200 tokens of context of which 400 were cached, and 80 tokens out —
        // so a billable cost of `80 + (1200 - 400) = 880` tokens and a 33% hit
        // rate, in tuix's `轮 · 工具 · dur · tokens · cached` shape.
        let s = fold(&conformance::facts());
        let ends: Vec<Vec<String>> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "turn_end")
            .map(|x| {
                x.block()
                    .content
                    .lines(&crate::block::RenderCtx::bare(60))
                    .iter()
                    .map(|l| l.plain())
                    .collect()
            })
            .collect();
        assert_eq!(ends.len(), 2, "two turns end in the corpus: {ends:?}");
        // All of it, on whichever row: at this width the figures can take the
        // row under the outcome.
        let said = ends[0].join("\n");
        for want in ["1 轮", "2 工具", "880 tokens", "本轮缓存 33%"] {
            assert!(said.contains(want), "{want} missing from {:?}", ends[0]);
        }
        // Turn 2 was a self-cancel: it draws no separator in the transcript now —
        // it closes on the composer instead.
        assert!(ends[1].is_empty(), "a cancel draws nothing: {:?}", ends[1]);
    }

    /// A turn's hit rate and cost are over every request it sent, not its last.
    ///
    /// The 14 requests of a real turn (session `ffd4eacc`): the 5th and 12th
    /// missed the cache outright, and the last hit 99.6%. The line closed on
    /// `13.11K tokens · 99% cached` — the last request's — while the footer,
    /// summing the same requests, said 66%. Summed, the two agree.
    /// Summed past `u32::MAX`, the figures stay true. Two requests of three
    /// billion each, two billion of each cached: 66% cached. A saturating `u32`
    /// stopped the input at 4.29 billion while the cache went on to four, and
    /// the line said 93%.
    #[test]
    fn a_turn_s_sums_do_not_stop_at_u32() {
        use atomcode_kernel::stream::TokenUsage;
        let reading = |round| SessionEvent::Usage {
            turn: 1,
            round,
            usage: TokenUsage {
                prompt: 3_000_000_000,
                completion: 0,
                cached: 2_000_000_000,
            },
        };
        let s = fold(&[
            SessionEvent::TurnStart { turn: 1 },
            reading(1),
            reading(2),
            SessionEvent::TurnEnd {
                turn: 1,
                stop: atomcode_harness::seams::StopReason::Stopped,
                error: None,
            },
        ]);
        let said: String = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "turn_end")
            .flat_map(|x| {
                x.block()
                    .content
                    .lines(&crate::block::RenderCtx::bare(120))
                    .into_iter()
                    .map(|l| l.plain())
            })
            .collect();
        assert!(said.contains("66%") && !said.contains("93%"), "{said}");
        assert!(said.contains("2000.00M tokens"), "{said}");
    }

    #[test]
    fn a_turn_s_cache_rate_is_over_every_request_not_its_last() {
        use atomcode_kernel::stream::TokenUsage;
        let readings: [(u32, u32, u32); 14] = [
            (36338, 256, 16896),
            (37353, 325, 35328),
            (38064, 455, 36352),
            (43694, 712, 43520),
            (54473, 189, 0),
            (54367, 58, 35200),
            (54448, 717, 35200),
            (88297, 215, 37376),
            (88455, 363, 87552),
            (89144, 886, 54272),
            (104592, 778, 88576),
            (138095, 716, 0),
            (173341, 85, 137728),
            (173171, 6732, 172544),
        ];
        let mut facts = vec![SessionEvent::TurnStart { turn: 1 }];
        for (round, (prompt, completion, cached)) in readings.iter().enumerate() {
            facts.push(SessionEvent::Usage {
                turn: 1,
                round: round as u32 + 1,
                usage: TokenUsage {
                    prompt: *prompt,
                    completion: *completion,
                    cached: *cached,
                },
            });
        }
        facts.push(SessionEvent::TurnEnd {
            turn: 1,
            stop: atomcode_harness::seams::StopReason::Stopped,
            error: None,
        });
        let s = fold(&facts);
        let said: String = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "turn_end")
            .flat_map(|x| {
                x.block()
                    .content
                    .lines(&crate::block::RenderCtx::bare(120))
                    .into_iter()
                    .map(|l| l.plain())
            })
            .collect::<Vec<_>>()
            .join("\n");
        // 780,544 of 1,173,832 sent were cached: 66%. Billable: the 393,288
        // that missed plus 12,487 generated.
        assert!(said.contains("66%"), "the turn's own rate: {said}");
        assert!(!said.contains("99%"), "not its last request's: {said}");
        assert!(said.contains("405.77K tokens"), "what it cost: {said}");
    }

    /// A turn's cost belongs to that turn. The corpus ends turn 1 and then
    /// starts turn 2 without either reporting usage, which is exactly the case
    /// where a fold that leaked would put turn 1's 1200 tokens on turn 2's line.
    #[test]
    fn one_turns_cost_never_lands_on_the_next_turns_line() {
        let mut facts = conformance::facts();
        // Give turn 2 a clean, figureless end.
        facts.push(SessionEvent::TurnEnd {
            turn: 2,
            stop: atomcode_harness::seams::StopReason::Stopped,
            error: None,
        });
        let s = fold(&facts);
        // A self-cancel draws no line now, so read the first line of each
        // turn-end block that draws one — the corpus's cancelled turn-2 end is
        // skipped, the clean one we pushed is not.
        let ends: Vec<String> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "turn_end")
            .filter_map(|x| {
                x.block()
                    .content
                    .lines(&crate::block::RenderCtx::bare(60))
                    .first()
                    .map(|l| l.plain())
            })
            .collect();
        let last = ends.last().expect("a turn-end that draws a line");
        // Turn 1 took `DONE_LABELS[0]` (`Done`); turn 2's Cancelled end did not
        // advance the rotation, so this clean turn-2 end is `DONE_LABELS[1]`.
        assert!(last.contains("Nailed it"), "{last:?}");
        for leaked in ["1200", "880", "tokens", "轮", "缓存"] {
            assert!(!last.contains(leaked), "{leaked} leaked into {last:?}");
        }
    }

    /// An incremental update names its task: `#2 → completed` alone says
    /// nothing once the panel is gone, so the row reads `#2 <task> → completed`.
    /// A number the plan has no task for keeps the tool's own words.
    #[test]
    fn a_todo_update_names_its_task() {
        let call = |id: &str, args: &str| SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: id.into(),
                name: "todowrite".into(),
                arguments: args.into(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        };
        let result = |id: &str, content: &str| SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: id.into(),
            content: content.into(),
            is_error: false,
            images: Vec::new(),
        };
        let facts = vec![
            SessionEvent::TurnStart { turn: 1 },
            call(
                "c1",
                r#"{"todos":[{"content":"write the migration","status":"completed"},
                    {"content":"run the tests","status":"in_progress"},
                    {"content":"wire it into CI","status":"pending"}]}"#,
            ),
            result("c1", "3 tasks"),
            call("c2", r#"{"action":"update","id":2,"status":"completed"}"#),
            result("c2", "#2 → completed"),
            call("c3", r#"{"action":"update","id":9,"status":"completed"}"#),
            result("c3", "#9 → completed"),
        ];
        let text = said(&fold(&facts));
        assert!(
            text.contains("#2 run the tests → completed"),
            "the update names its task:\n{text}"
        );
        assert!(
            text.contains("#9 → completed"),
            "no task for the number: the tool's words stand:\n{text}"
        );

        // One reply that updates #2 and then replans: the update was written
        // against the old list, and its row names the old #2.
        let facts = vec![
            SessionEvent::TurnStart { turn: 1 },
            call(
                "c1",
                r#"{"todos":[{"content":"old one","status":"completed"},
                    {"content":"old two","status":"in_progress"}]}"#,
            ),
            result("c1", "2 tasks"),
            SessionEvent::AssistantMessage {
                turn: 1,
                round: 2,
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![
                    atomcode_kernel::tool::ToolCall {
                        id: "c2".into(),
                        name: "todowrite".into(),
                        arguments: r#"{"action":"update","id":2,"status":"completed"}"#.into(),
                    },
                    atomcode_kernel::tool::ToolCall {
                        id: "c3".into(),
                        name: "todowrite".into(),
                        arguments: r#"{"todos":[{"content":"new one","status":"pending"},
                            {"content":"new two","status":"pending"}]}"#
                            .into(),
                    },
                ],
                reasoning_blocks: Vec::new(),
                meta: None,
            },
            result("c2", "#2 → completed"),
            result("c3", "2 tasks"),
        ];
        let text = said(&fold(&facts));
        assert!(
            text.contains("#2 old two → completed"),
            "the list it was written against, not the replan after it:\n{text}"
        );
    }

    /// A turn the model ended on its own says so, not "done", while the task list
    /// it kept still has open items — and says how many, so the person knows
    /// work is left. Reported against 5.1.0: a 45-minute turn ended on "运行测试："
    /// with no call, and the line read `✓ Served` over a list that was not done.
    ///
    /// The count follows the plan through an undo the way the panel does: the
    /// turn that finished the list is taken back, and the list is open again.
    #[test]
    fn a_turn_that_stops_with_the_list_open_says_so_instead_of_done() {
        use atomcode_harness::seams::StopReason;
        use atomcode_harness::session::RewindScope;
        let plan = |turn: u64, id: &str, todos: &str| SessionEvent::AssistantMessage {
            turn,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: id.into(),
                name: "todowrite".into(),
                arguments: format!(r#"{{"todos":{todos}}}"#),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        };
        let end = |turn: u64| SessionEvent::TurnEnd {
            turn,
            stop: StopReason::Stopped,
            error: None,
        };
        let facts = vec![
            SessionEvent::TurnStart { turn: 1 },
            plan(
                1,
                "c1",
                r#"[{"content":"write the migration","status":"completed"},
                    {"content":"run the tests","status":"in_progress"},
                    {"content":"wire it into CI","status":"pending"}]"#,
            ),
            end(1),
            // `conformance::logged` numbers from one: this `TurnStart` is seq 4.
            SessionEvent::TurnStart { turn: 2 },
            plan(
                2,
                "c2",
                r#"[{"content":"write the migration","status":"completed"},
                    {"content":"run the tests","status":"completed"},
                    {"content":"wire it into CI","status":"completed"}]"#,
            ),
            end(2),
            SessionEvent::TurnStart { turn: 3 },
            SessionEvent::Rewound {
                turn: 3,
                to: 4,
                scope: RewindScope::Conversation,
            },
            // The turn after the undo works the list again (an update, not a
            // new plan), so it is a turn that stopped on the list.
            SessionEvent::AssistantMessage {
                turn: 3,
                round: 1,
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![atomcode_kernel::tool::ToolCall {
                    id: "c3".into(),
                    name: "todowrite".into(),
                    arguments: r#"{"action":"update","id":2,"status":"in_progress"}"#.into(),
                }],
                reasoning_blocks: Vec::new(),
                meta: None,
            },
            end(3),
        ];
        let s = fold(&facts);
        let ends: Vec<String> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "turn_end")
            .filter_map(|x| {
                x.block()
                    .content
                    .lines(&crate::block::RenderCtx::bare(120))
                    .first()
                    .map(|l| l.plain())
            })
            .collect();
        assert_eq!(ends.len(), 3, "{ends:?}");
        assert!(ends[0].contains("还有 2 项没完成"), "{:?}", ends[0]);
        assert!(!ends[0].contains("Done"), "{:?}", ends[0]);
        // The open stop did not spend a rotation slot: the first clean finish is `Done`.
        assert!(ends[1].contains("Done"), "{:?}", ends[1]);
        assert!(
            ends[2].contains("还有 2 项没完成"),
            "undo reopens the list: {:?}",
            ends[2]
        );
    }

    /// Two stops that leave the list open and are not the model walking away
    /// from it, so the line does not tell the person to send "继续": a reply
    /// that ends on a question (the person owes an answer), and a later turn
    /// that never touched the list (a question about something else, answered).
    /// The same two conditions keep the runtime from nudging those stops.
    #[test]
    fn a_question_or_an_unrelated_turn_is_not_called_stopped_with_work_open() {
        use atomcode_harness::seams::StopReason;
        let reply = |turn: u64, text: &str, calls: Vec<atomcode_kernel::tool::ToolCall>| {
            SessionEvent::AssistantMessage {
                turn,
                round: 1,
                text: text.into(),
                reasoning: String::new(),
                tool_calls: calls,
                reasoning_blocks: Vec::new(),
                meta: None,
            }
        };
        let plan = atomcode_kernel::tool::ToolCall {
            id: "c1".into(),
            name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"write the migration","status":"completed"},
                {"content":"wire it into CI","status":"pending"}]}"#
                .into(),
        };
        let end = |turn: u64| SessionEvent::TurnEnd {
            turn,
            stop: StopReason::Stopped,
            error: None,
        };
        let facts = vec![
            SessionEvent::TurnStart { turn: 1 },
            reply(1, "", vec![plan]),
            reply(1, "迁移写好了。\n要我接着改 CI 配置吗？", Vec::new()),
            end(1),
            SessionEvent::TurnStart { turn: 2 },
            reply(2, "foo 把配置读进来,再交给 loader。", Vec::new()),
            end(2),
        ];
        let s = fold(&facts);
        let ends: Vec<String> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "turn_end")
            .filter_map(|x| {
                x.block()
                    .content
                    .lines(&crate::block::RenderCtx::bare(120))
                    .first()
                    .map(|l| l.plain())
            })
            .collect();
        assert_eq!(ends.len(), 2, "{ends:?}");
        for end in &ends {
            assert!(!end.contains("没完成"), "{end:?}");
        }
    }

    /// The line that closes a turn says when it ended, read off the `TurnEnd`
    /// fact's own stamp — so a resumed or replayed session shows when its turns
    /// really ended, not when they were drawn again.
    #[test]
    fn a_turns_closing_line_says_when_it_ended() {
        let at = 1_790_000_000_000_u64;
        let stamped = |seq: u64, event: SessionEvent| atomcode_harness::session::LoggedEvent {
            seq,
            at,
            event,
        };
        let mut s = Stream::new();
        let t = Transcript::default();
        for (i, fact) in [
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::TurnEnd {
                turn: 1,
                stop: atomcode_harness::seams::StopReason::Stopped,
                error: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let mut w = s.writer(ID);
            t.absorb(&stamped(i as u64 + 1, fact), &mut w);
        }
        let clock = crate::content::clock_of(at).expect("a real stamp reads as a time");
        assert!(said(&s).contains(&clock), "{} lacks {clock}", said(&s));
    }

    #[test]
    fn calls_appear_in_the_order_they_were_asked_for() {
        let s = fold(&conformance::facts());
        let ids: Vec<String> = s
            .slots()
            .iter()
            .filter_map(|x| match x {
                Slot::Settled(s) if s.block().kind() == "tool_call" => {
                    Some(s.block().content.lines(&crate::block::RenderCtx::bare(80))[0].plain())
                }
                _ => None,
            })
            .collect();
        assert!(ids[0].contains("a.rs") && ids[1].contains("b.rs"));
    }

    #[test]
    fn a_turn_cut_short_marks_its_call_interrupted_not_failed() {
        let mut facts = conformance::facts()[..8].to_vec(); // through the calls
        facts.push(SessionEvent::TurnEnd {
            turn: 1,
            stop: atomcode_harness::seams::StopReason::Cancelled,
            error: None,
        });
        let s = fold(&facts);
        let line = s
            .slots()
            .iter()
            .find(|x| x.block().kind() == "tool_call")
            .unwrap()
            .block()
            .content
            .lines(&crate::block::RenderCtx::bare(60))[1]
            .plain();
        assert!(
            line.contains("已中断") && !line.contains("失败"),
            "a cut turn interrupts its calls, it does not fail them: {line}"
        );
        assert!(
            s.slots().iter().all(|x| x.is_settled()),
            "a turn that ended leaves nothing open"
        );
    }

    /// A reply the person stopped: live, the chunks drew it and the fact only
    /// settles it — one block, not two; replayed from a log that keeps no
    /// chunks, the fact is what draws it.
    #[test]
    fn a_stopped_reply_is_drawn_once_live_and_again_on_replay() {
        let partial = SessionEvent::PartialReply {
            turn: 1,
            round: 1,
            text: "I was saying".into(),
            reasoning: "thinking".into(),
        };
        let said = |s: &Stream| -> Vec<String> {
            s.slots()
                .iter()
                .filter(|x| x.block().kind() == "assistant")
                .map(|x| x.block().content.lines(&crate::block::RenderCtx::bare(80))[0].plain())
                .collect()
        };

        let live = fold(&[
            SessionEvent::AssistantChunk {
                turn: 1,
                round: 1,
                delta: "I was saying".into(),
                reasoning: false,
            },
            partial.clone(),
        ]);
        assert_eq!(said(&live), vec!["I was saying".to_string()]);
        assert!(live.slots().iter().all(|x| !x.is_live()), "settled");

        let replayed = fold(&[partial]);
        assert_eq!(said(&replayed), vec!["I was saying".to_string()]);
        assert!(kinds(&replayed).contains(&"reasoning"));
    }

    #[test]
    fn every_kind_of_fact_reaches_the_screen_or_is_deliberately_silent() {
        let s = fold(&conformance::facts());
        let k = kinds(&s);
        for want in [
            "user",
            "assistant",
            "reasoning",
            "tool_call",
            "notice",
            "choice",
            "injected:reminder",
            "injected:peer",
            "turn_end",
        ] {
            assert!(k.contains(&want), "`{want}` never appeared: {k:?}");
        }

        // In the stream is not the same as on the screen, and the difference is
        // the whole point of keying an injection by its origin: both of these are
        // facts, both are in the content hashes, and only one of them is painted.
        // Asserted here, next to the kinds, because a block that stopped being
        // produced at all would otherwise pass the presentation test by simply
        // never arriving.
        let p = crate::host::Presentation::default_folds();
        assert!(
            p.is_hidden("injected:reminder"),
            "the environment's own reminder is on screen by default"
        );
        assert!(
            !p.is_hidden("injected:peer"),
            "a teammate's report is not the environment talking to itself"
        );
    }
}
