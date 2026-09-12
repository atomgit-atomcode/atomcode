//! The conversation, as an irreversible stream of blocks.
//!
//! One producer, six kinds of block. The awkward part is that a tool call is
//! **one block from two facts** — the call arrives with the assistant message
//! and the result arrives later, possibly out of order relative to its
//! siblings. Correlating them by `call_id` is what keeps the transcript one
//! row per call instead of two.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use atomcode_harness::session::{InjectionOrigin, SessionEvent};

use crate::block::{BlockId, Coord, StreamWriter};
use crate::content::{
    InjectedBlock, ModelSaid, ModelThought, NoticeBlock, Outcome, ToolCallBlock, TurnEndBlock,
    TurnStats, UserSaid,
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
        InjectionOrigin::CompactionSummary => "compaction summary".into(),
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
                        origin: origin_label(origin),
                        text: text.clone(),
                    }),
                );
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
        for want in ["1 步", "入 1200", "出 80", "缓存 33%"] {
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
                Slot::Settled(b) if b.kind() == "tool_call" => Some(b.content.lines(80)[0].plain()),
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
            "injected",
            "turn_end",
        ] {
            assert!(k.contains(&want), "`{want}` never appeared: {k:?}");
        }
    }
}
