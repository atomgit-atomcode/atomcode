//! The conversation, as an irreversible stream of blocks.
//!
//! One producer, six kinds of block. The awkward part is that a tool call is
//! **one block from two facts** — the call arrives with the assistant message
//! and the result arrives later, possibly out of order relative to its
//! siblings. Correlating them by `call_id` is what keeps the transcript one
//! row per call instead of two.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use atomcode_harness::session::SessionEvent;

use crate::block::{BlockId, Coord, StreamWriter};
use crate::content::{
    InjectedBlock, ModelSaid, ModelThought, NoticeBlock, Outcome, ToolCallBlock, TurnEndBlock,
    UserSaid,
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
                        origin: format!("{origin:?}").to_lowercase(),
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
                out.emit(
                    at,
                    Arc::new(TurnEndBlock {
                        stop: format!("{stop:?}"),
                        error: error.clone(),
                    }),
                );
            }

            // Turn and step boundaries are coordinates, not blocks; request
            // headers and usage are the status module's business.
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
        assert_eq!(calls.len(), 2, "two calls, two blocks — not four");
        assert!(calls.iter().all(|c| c.is_settled()));
        let rendered: Vec<String> = calls
            .iter()
            .map(|c| c.block().content.lines(60)[0].plain())
            .collect();
        assert!(rendered[0].starts_with('✓'), "{rendered:?}");
        assert!(rendered[1].starts_with('✗'), "the failing one is marked");
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
            .lines(60)[0]
            .plain();
        assert!(line.starts_with('—'), "interrupted, not ✗: {line}");
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
