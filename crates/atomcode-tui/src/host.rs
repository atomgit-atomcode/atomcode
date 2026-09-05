//! The host: the four things no module may own.
//!
//! It holds the surface, the layout, focus, and the redraw cycle — and it knows
//! **no module by name**. Composition is: walk the region tree for rects, look
//! each leaf up in the registry, ask it to draw into its rect, check nothing
//! spilled. That last step is the pixel-level verdict on spatial
//! composability, and it runs in every frame in debug builds.

use std::sync::{Arc, Mutex, RwLock};

use atomcode_harness::session::SessionEvent;

use crate::block::{Slot, Stream};
use crate::frame::{Frame, Line, Rect};
use crate::module::{Height, Modules};
use crate::moment::Moment;
use crate::region::Region;

/// Which blocks are shown how. Kept here, keyed by id, rather than on the block
/// — which is what makes "folding does not change content" structural.
#[derive(Default)]
pub struct Presentation {
    folded_kinds: Vec<&'static str>,
}

impl Presentation {
    /// Kinds shown as one line unless expanded. Reasoning and tool calls are
    /// folded by default because a transcript is read for the answer, not for
    /// the working.
    pub fn default_folds() -> Self {
        Self {
            folded_kinds: vec!["reasoning", "tool_call"],
        }
    }
    pub fn is_folded(&self, kind: &str) -> bool {
        self.folded_kinds.contains(&kind)
    }
    pub fn toggle(&mut self, kind: &'static str) {
        match self.folded_kinds.iter().position(|k| *k == kind) {
            Some(i) => {
                self.folded_kinds.remove(i);
            }
            None => self.folded_kinds.push(kind),
        }
    }
}

/// Everything the screen is composed from.
pub struct Host {
    pub stream: RwLock<Stream>,
    /// Slash commands, contributed by rows.
    pub commands: Arc<crate::command::Commands>,
    /// At most one modal. Focus is arbitration, not composition.
    pub overlays: Arc<crate::overlay::Overlays>,
    /// Questions waiting for the person. Rendered as a live block at the foot
    /// of the stream, and given first refusal on every key while it is there.
    pub asks: Arc<crate::ask::Asks>,
    pub modules: Arc<Modules>,
    pub layout: Arc<crate::layout::Layout>,
    pub moment: RwLock<Moment>,
    pub presentation: RwLock<Presentation>,
    /// Frames composed so far. Only counted, not kept — the surface keeps them
    /// when it is the headless one.
    painted: Mutex<u64>,
}

impl Host {
    pub fn new(modules: Arc<Modules>, layout: Region) -> Self {
        let layout_svc = Arc::new(crate::layout::Layout::new(layout));
        Self {
            stream: RwLock::new(Stream::new()),
            // Empty, like the module registry beside it. Command sets arrive as
            // rows (`crate::rows`); a Host that pre-filled this would make
            // `[[remove]] id = "tui-commands-tree"` a lie.
            commands: Arc::new(crate::command::Commands::new()),
            overlays: Arc::new(crate::overlay::Overlays::new()),
            asks: crate::ask::Asks::new(),
            modules: modules.clone(),
            layout: layout_svc.clone(),
            moment: RwLock::new(Moment::default()),
            presentation: RwLock::new(Presentation::default_folds()),
            painted: Mutex::new(0),
        }
    }

    /// Deliver one committed fact to every module.
    ///
    /// Producers first, then views: a view that reacts to the same fact should
    /// see a screen whose stream already contains it.
    pub fn absorb(&self, fact: &SessionEvent) {
        {
            let mut stream = self.stream.write().expect("stream poisoned");
            for p in self.modules.producers() {
                let mut w = stream.writer(p.id());
                p.absorb(fact, &mut w);
            }
        }
        for id in self.modules.view_ids() {
            if let Some(v) = self.modules.view(id) {
                v.absorb(fact);
            }
        }
    }

    pub fn painted(&self) -> u64 {
        *self.painted.lock().expect("counter poisoned")
    }

    /// Render the stream's tail into `rect`.
    ///
    /// Bottom-anchored: what a person is reading is the newest thing. Blocks
    /// are rendered newest-first until the rect is full, then reversed — so the
    /// cost is O(what fits), not O(the conversation).
    fn stream_lines(&self, rect: Rect) -> Vec<Line> {
        let stream = self.stream.read().expect("stream poisoned");
        let pres = self.presentation.read().expect("presentation poisoned");
        let mut out: Vec<Line> = Vec::new();
        let want = rect.h as usize;

        // A question in flight sits at the foot of the stream. It is not in the
        // stream itself: it has no answer yet, and a block whose content is
        // still to be decided has not settled — putting it in would mean
        // amending a settled block the moment it is answered.
        if let Some((_, question, options)) = self.asks.peek() {
            let pending = crate::content::ChoiceBlock {
                question,
                options,
                answer: None,
            };
            let mut lines = crate::block::Content::lines(&pending, rect.w);
            lines.reverse();
            for line in lines {
                if out.len() < want {
                    out.push(line);
                }
            }
        }
        let scroll = self.moment.read().expect("moment poisoned").scroll.0;
        let mut skipped = 0usize;
        let _ = &skipped;

        for slot in stream.slots().iter().rev() {
            let block = slot.block();
            let mut lines = if pres.is_folded(block.kind()) && !block.content.always_open() {
                vec![block.content.summary(rect.w)]
            } else {
                block.content.lines(rect.w)
            };
            if lines.is_empty() {
                continue;
            }
            lines.reverse();
            for line in lines {
                if skipped < scroll {
                    skipped += 1;
                    continue;
                }
                if out.len() >= want {
                    break;
                }
                out.push(line);
            }
            if out.len() >= want {
                break;
            }
        }
        out.reverse();
        // Push the content to the bottom of the rect when there is not enough
        // of it, so the newest line is always where the eye expects it.
        let pad = want.saturating_sub(out.len());
        let mut padded = vec![Line::empty(); pad];
        padded.extend(out);
        padded
    }

    /// Compose one frame.
    pub fn compose(&self, size: (u16, u16)) -> Frame {
        let (w, h) = size;
        let mut frame = Frame::new(w, h);
        let moment = self.moment.read().expect("moment poisoned").clone();

        let layout = self.layout.tree();
        let modules = self.modules.clone();
        let pruned = layout.prune(&|id| modules.has_view(id));

        // Ask each module how much room it would like, then let the tree
        // decide. Requests, not seizures.
        let asked = |id: &str| -> u16 {
            modules
                .view(id)
                .map(|v| match v.height() {
                    Height::Fixed(n) | Height::Hug(n) => n,
                    Height::Fill => 1,
                })
                .unwrap_or(1)
        };
        for (region, rect) in pruned.layout_with(Rect::sized(w, h), &asked) {
            if rect.is_empty() {
                continue;
            }
            match region {
                Region::Stream => {
                    frame.place("stream", rect, self.stream_lines(rect));
                }
                Region::Module(id) => {
                    let Some(view) = modules.view(&id) else {
                        continue;
                    };
                    let vp = crate::moment::Viewport::new(rect, &moment);
                    let mut lines = view.render(&vp);
                    // Arbitration is the host's, so a module can request but
                    // never seize: too many lines are clipped, never allowed to
                    // push a neighbour off the screen.
                    let cap = match view.height() {
                        Height::Fixed(n) => n.min(rect.h),
                        Height::Hug(n) => n.min(rect.h),
                        Height::Fill => rect.h,
                    } as usize;
                    lines.truncate(cap);
                    if id == crate::modules::input::ID {
                        frame.cursor = Some(crate::modules::input::caret(&moment, rect));
                    }
                    frame.place(id, rect, lines);
                }
                _ => {}
            }
        }

        // A modal is drawn last, over everything, in a box of its own.
        if let Some(modal) = self.overlays.current() {
            let rect = crate::overlay::frame_rect(Rect::sized(w, h), modal.size());
            if !rect.is_empty() {
                let vp = crate::moment::Viewport::new(
                    Rect::new(
                        rect.x + 1,
                        rect.y + 1,
                        rect.w.saturating_sub(2),
                        rect.h.saturating_sub(2),
                    ),
                    &moment,
                );
                let body = modal.render(&vp);
                frame.place(
                    modal.id(),
                    rect,
                    crate::overlay::framed(&modal.title(), body, rect),
                );
                frame.cursor = None;
            }
        }

        debug_assert!(
            frame.containment_violations().is_empty(),
            "a module drew outside its rect: {:?}",
            frame.containment_violations()
        );
        *self.painted.lock().expect("counter poisoned") += 1;
        frame
    }

    /// How many rendered lines the stream currently holds, for scroll bounds.
    pub fn stream_height(&self, width: u16) -> usize {
        let stream = self.stream.read().expect("stream poisoned");
        let pres = self.presentation.read().expect("presentation poisoned");
        stream
            .slots()
            .iter()
            .map(|s| {
                let b = s.block();
                if pres.is_folded(b.kind()) {
                    1
                } else {
                    b.content.lines(width).len()
                }
            })
            .sum()
    }

    pub fn live_blocks(&self) -> usize {
        self.stream
            .read()
            .expect("stream poisoned")
            .slots()
            .iter()
            .filter(|s| matches!(s, Slot::Live(_)))
            .count()
    }
}

/// The shipped layout: the conversation, a status bar, a prompt.
/// Stream, composer, status line — in that order, top to bottom.
///
/// The status line is *last*. `atomcode-tuix` puts it there and dims it: it is
/// the thing you glance at, and the top of the screen belongs to the
/// conversation. A reverse-video bar across the top is what an editor does.
pub fn default_layout() -> Region {
    use crate::region::{Constraint, Dir};
    Region::split(
        Dir::Vertical,
        Constraint::Fill,
        Region::Stream,
        Region::split(
            Dir::Vertical,
            Constraint::Fill,
            Region::view(crate::modules::input::ID),
            Region::view(crate::modules::status::ID),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance;
    use crate::module::Mounted;
    use crate::modules::{input, status, transcript};

    fn host() -> Host {
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<input::Input>::new()))
            .unwrap();
        Host::new(mods, default_layout())
    }

    fn fed() -> Host {
        let h = host();
        for f in conformance::facts() {
            h.absorb(&f);
        }
        h
    }

    #[test]
    fn a_frame_has_a_status_bar_a_conversation_and_a_prompt() {
        let h = fed();
        let f = h.compose((80, 24));
        assert!(f.part("status").is_some());
        assert!(f.part("stream").is_some());
        assert!(f.part("input").is_some());
        assert!(f.containment_violations().is_empty());
        let text = f.rows().join("\n");
        assert!(text.contains("fix the build"), "the user's words:\n{text}");
        assert!(text.contains("Fixed it"), "the model's answer");
        assert!(text.contains("read_file"), "the tools it used");
    }

    #[test]
    fn nothing_ever_draws_outside_its_rect_at_any_size() {
        let h = fed();
        for w in [1u16, 2, 10, 40, 200] {
            for ht in [1u16, 2, 5, 24, 60] {
                let f = h.compose((w, ht));
                assert!(
                    f.containment_violations().is_empty(),
                    "{w}×{ht}: {:?}",
                    f.containment_violations()
                );
                assert_eq!(f.rows().len(), ht as usize);
            }
        }
    }

    #[test]
    fn the_newest_line_is_at_the_bottom() {
        let h = fed();
        // Assert on the stream's own part rather than on the whole screen: how
        // many rows the prompt happens to take is the input module's business.
        let frame = h.compose((80, 24));
        let stream = frame.part("stream").expect("the stream is placed");
        let last = stream
            .lines
            .iter()
            .rev()
            .find(|l| !l.plain().trim().is_empty())
            .expect("something was said");
        assert!(
            last.plain().contains("Cancelled"),
            "bottom-anchored, newest last: {:?}",
            last.plain()
        );
    }

    #[test]
    fn reasoning_is_folded_by_default_and_expands_without_changing_content() {
        let h = fed();
        let folded = h.compose((80, 40)).rows().join("\n");
        assert!(folded.contains("思考"), "folded to a summary");
        assert!(!folded.contains("hmm"), "the words are hidden, not gone");

        let hashes_before: Vec<_> = h
            .stream
            .read()
            .unwrap()
            .settled_hashes()
            .into_iter()
            .collect();
        h.presentation.write().unwrap().toggle("reasoning");
        let open = h.compose((80, 40)).rows().join("\n");
        assert!(open.contains("hmm"), "expanding shows them");
        assert_eq!(
            h.stream.read().unwrap().settled_hashes(),
            hashes_before,
            "presentation changed; content did not"
        );
    }

    #[test]
    fn an_unmounted_module_leaves_the_others_alone() {
        let h = fed();
        let with = h.compose((80, 24));
        h.modules.remove_view("status");
        let without = h.compose((80, 24));
        assert!(without.part("status").is_none());
        assert!(without.part("input").is_some(), "the rest still draws");
        assert!(with.rows().len() == without.rows().len());
    }

    #[test]
    fn a_module_mounted_mid_conversation_appears_on_the_next_frame() {
        let h = fed();
        assert!(h.compose((80, 24)).part("mascot").is_none());
        h.modules
            .add_view(Arc::new(Mounted::<status::Mascot>::new()))
            .unwrap();
        h.layout.set(Region::split(
            crate::region::Dir::Vertical,
            crate::region::Constraint::Cells(1),
            Region::view("mascot"),
            default_layout(),
        ));
        assert!(
            h.compose((80, 24)).part("mascot").is_some(),
            "the registry is read fresh, not snapshotted"
        );
    }
}

#[cfg(test)]
mod demo {
    //! Not an assertion — a way to look at a real frame. `cargo test -p
    //! atomcode-tui --lib demo -- --nocapture` prints what a person would see.
    use super::*;
    use crate::conformance;
    use crate::module::Mounted;
    use crate::modules::{input, status, transcript};

    #[test]
    fn print_a_frame() {
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<input::Input>::new()))
            .unwrap();
        let h = Host::new(mods, default_layout());
        for f in conformance::facts() {
            h.absorb(&f);
        }
        // A realistic answer: prose, a list, and code. Built from a slice so
        // the source indentation of this file cannot leak into the fixture.
        let answer = [
            "找到了 **两处**问题:",
            "",
            "1. `parse` 没有检查空输入",
            "2. 重试没有上界",
            "",
            "```rust",
            "fn parse(s: &str) -> Result<Cfg> {",
            "    if s.is_empty() { return Err(Empty); }",
            "}",
            "```",
            "",
            "要我改吗?",
        ]
        .join("\n");
        h.absorb(&atomcode_harness::session::SessionEvent::AssistantMessage {
            turn: 2,
            round: 1,
            text: answer,
            reasoning: String::new(),
            tool_calls: Vec::new(),
        });
        h.moment.write().unwrap().input = "接下来呢".into();
        let frame = h.compose((78, 20));
        println!("\n┌{}┐", "─".repeat(78));
        for row in frame.rows() {
            // Every row is exactly the screen's width in *cells*. Asserting it
            // here as well as printing it: a demo that quietly drifted from the
            // real geometry would be worse than no demo.
            assert_eq!(
                crate::width::str_width(&row),
                78,
                "row is {} cells, not 78: {row:?}",
                crate::width::str_width(&row)
            );
            println!("│{row}│");
        }
        println!("└{}┘\n", "─".repeat(78));
    }
}
