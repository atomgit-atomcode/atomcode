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
}

/// Turns session facts into what a person reads.
#[derive(Default)]
pub struct Transcript {
    open: Mutex<Open>,
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
    }
}

impl Producer for Transcript {
    fn id(&self) -> &'static str {
        ID
    }

    fn absorb(&self, fact: &SessionEvent, out: &mut StreamWriter<'_>) {
        let mut open = self.open.lock().expect("transcript poisoned");
        let at = Coord::new(fact.turn(), 0);
        match fact {
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
                // Taken, not read: a turn's cost is spent when the turn ends,
                // and the next `TurnStart` would otherwise be the only thing
                // standing between one turn's figures and the next turn's line.
                let stats = std::mem::take(&mut open.stats);
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
                    }),
                );
            }

            // What the turn cost, in the two facts that carry it. `step` and
            // `round` are one counter in the loop, so the steps are read off
            // the same number the requests are numbered with.
            SessionEvent::StepEnd { step, .. } => open.stats.steps = *step,

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
        for f in facts {
            let mut w = s.writer(ID);
            t.absorb(f, &mut w);
        }
        s
    }

    fn kinds(s: &Stream) -> Vec<&'static str> {
        s.slots().iter().map(|x| x.block().kind()).collect()
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
                crate::block::Content::lines(&*x.block().content, 80)
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
        assert_eq!(text[0].block().content.lines(80)[0].plain(), "Looking.");
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
                    .lines(60)
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
        // The corpus' turn 1: one step, one request reporting 1200 tokens of
        // context of which 400 were cached, and 80 tokens out.
        let s = fold(&conformance::facts());
        let ends: Vec<String> = s
            .slots()
            .iter()
            .filter(|x| x.block().kind() == "turn_end")
            .map(|x| x.block().content.lines(60)[0].plain())
            .collect();
        assert_eq!(ends.len(), 2, "two turns end in the corpus: {ends:?}");
        for want in ["1 步", "入 1200", "出 80", "缓存 33.33%"] {
            assert!(ends[0].contains(want), "{want} missing from {:?}", ends[0]);
        }
        // Turn 2 reported nothing, so its line is the outcome alone — not a row
        // of zeroes.
        assert!(!ends[1].contains("步"), "{:?}", ends[1]);
        assert!(!ends[1].contains("入"), "{:?}", ends[1]);
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
            .map(|x| x.block().content.lines(60)[0].plain())
            .collect();
        let last = ends.last().expect("turn 2 ends");
        assert!(last.contains("完成"), "{last:?}");
        for leaked in ["1200", "80", "缓存", "步"] {
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
                    Some(s.block().content.lines(80)[0].plain())
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
            .lines(60)[1]
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
