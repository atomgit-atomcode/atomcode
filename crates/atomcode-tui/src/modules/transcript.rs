//! The conversation, as an irreversible stream of blocks.
//!
//! One producer, six kinds of block. The awkward part is that a tool call is
//! **one block from two facts** — the call arrives with the assistant message
//! and the result arrives later, possibly out of order relative to its
//! siblings. Correlating them by `call_id` is what keeps the transcript one
//! row per call instead of two.

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
    }

    fn absorb(&self, logged: &atomcode_harness::session::LoggedEvent, out: &mut StreamWriter<'_>) {
        let fact = &logged.event;
        let mut open = self.open.lock().expect("transcript poisoned");
        let at = Coord::new(fact.turn(), 0);
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
            }
            SessionEvent::Rewound { to, scope, .. } => {
                let to_turn = self
                    .turns
                    .lock()
                    .expect("transcript poisoned")
                    .iter()
                    .find(|(seq, _)| *seq == *to)
                    .map(|(_, turn)| *turn);
                out.emit(
                    at,
                    Arc::new(crate::content::RewoundBlock {
                        to_turn,
                        scope: *scope,
                    }),
                );
            }
            SessionEvent::UserMessage { text, .. } => {
                out.emit(at, Arc::new(UserSaid(text.clone())));
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
                    let block = ToolCallBlock::pending(&call.id, &call.name, &call.arguments);
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
                let outcome = if *is_error {
                    Outcome::Failed(content.clone())
                } else {
                    Outcome::Ok(content.clone())
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

            SessionEvent::Notice { detail, .. } => {
                out.emit(
                    at,
                    Arc::new(NoticeBlock {
                        detail: detail.clone(),
                    }),
                );
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
                        detail: format!("已把这里之前的对话压成一段摘要(到 #{through})"),
                    }),
                );
            }
            SessionEvent::MessagesRewritten { texts, .. } => {
                out.emit(
                    at,
                    Arc::new(NoticeBlock {
                        detail: format!(
                            "模型看到的 {} 处工具输出被就地换短了;这里显示的仍是原文",
                            texts.len()
                        ),
                    }),
                );
            }
            SessionEvent::ToolResultsStubbed { through, .. } => {
                out.emit(
                    at,
                    Arc::new(NoticeBlock {
                        detail: format!("到 #{through} 为止的工具结果没有再发给模型"),
                    }),
                );
            }
            SessionEvent::RateLimitPaused { pause, .. } => {
                let mut detail = format!("被限速,等到 {}", pause.reset_at_display);
                if let Some(message) = &pause.server_message {
                    detail.push_str(&format!(" · {message}"));
                }
                out.emit(at, Arc::new(NoticeBlock { detail }));
            }
            // A member of this session's team stopped for good
            // (`docs/adr/0024` §13). The team panel says so while it is
            // mounted; the conversation should say it too, because a lead whose
            // member is gone is reading a conversation that will not continue.
            SessionEvent::Stopped { .. } => {
                out.emit(
                    at,
                    Arc::new(NoticeBlock {
                        detail: "这个成员已经结束,不会再说话了".into(),
                    }),
                );
            }

            SessionEvent::Injected { text, origin, .. } => {
                out.emit(
                    at,
                    Arc::new(InjectedBlock {
                        kind: origin_kind(origin),
                        origin: origin_label(origin),
                        text: text.clone(),
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
            SessionEvent::Answered { answer, .. } => {
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
                    Some((id, asked)) => {
                        out.amend(id, Arc::new(card_for(&asked, Some(said))));
                        out.settle(id);
                    }
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
                // A clean stop takes the next rotation slot and advances it; every
                // other outcome leaves the rotation where it is (its label is
                // ignored) so the celebratory verbs are not burned on failures.
                let done_index = {
                    let mut seq = self.done_seq.lock().expect("transcript poisoned");
                    let idx = *seq;
                    if matches!(stop, atomcode_harness::seams::StopReason::Stopped) {
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
                open.stats.tools += *tool_calls;
            }

            // A round's usage, merged by the loop into one figure per round.
            SessionEvent::Usage { usage, .. } => {
                // `prompt` is the whole context this request sent — and the two
                // obvious folds are both wrong. Summing counts the same opening
                // prefix once per round; a running max cannot go down, so a turn
                // whose context was compacted would keep reporting the size
                // before the compaction. The last reading is the true one.
                //
                // `cached` is a part of that same request, so it is taken from
                // the same reading rather than kept on its own: a hit rate is
                // only meaningful against the request it came from.
                open.stats.prompt = usage.prompt;
                open.stats.cached = usage.cached;
                // Output is the one figure that does add up: each round
                // generated its own, and the loop has already folded whatever
                // the provider re-sent within a round.
                open.stats.completion += usage.completion;
            }

            // Turn and step boundaries are coordinates, not blocks; request
            // headers are the status module's business.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(
            drawn.contains("被限速") && drawn.contains("14:30"),
            "it says what happened and until when:\n{drawn}"
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
        let ends: Vec<String> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "turn_end")
            .map(|x| x.block().content.lines(&crate::block::RenderCtx::bare(60))[0].plain())
            .collect();
        assert_eq!(ends.len(), 2, "two turns end in the corpus: {ends:?}");
        for want in ["1 轮", "2 工具", "880 tokens", "33% cached"] {
            assert!(ends[0].contains(want), "{want} missing from {:?}", ends[0]);
        }
        // Turn 2 reported nothing, so its line is the outcome alone — not a row
        // of zeroes.
        assert!(!ends[1].contains("轮"), "{:?}", ends[1]);
        assert!(!ends[1].contains("tokens"), "{:?}", ends[1]);
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
        let ends: Vec<String> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "turn_end")
            .map(|x| x.block().content.lines(&crate::block::RenderCtx::bare(60))[0].plain())
            .collect();
        let last = ends.last().expect("turn 2 ends");
        // Turn 1 took `DONE_LABELS[0]` (`Done`); turn 2's Cancelled end did not
        // advance the rotation, so this clean turn-2 end is `DONE_LABELS[1]`.
        assert!(last.contains("Nailed it"), "{last:?}");
        for leaked in ["1200", "880", "tokens", "轮", "cached"] {
            assert!(!last.contains(leaked), "{leaked} leaked into {last:?}");
        }
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
