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
            folded_kinds: vec!["reasoning"],
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
    pub modules: Arc<Modules>,
    pub layout: RwLock<Region>,
    pub moment: RwLock<Moment>,
    pub presentation: RwLock<Presentation>,
    /// Frames composed so far. Only counted, not kept — the surface keeps them
    /// when it is the headless one.
    painted: Mutex<u64>,
}

impl Host {
    pub fn new(modules: Arc<Modules>, layout: Region) -> Self {
        Self {
            stream: RwLock::new(Stream::new()),
            modules,
            layout: RwLock::new(layout),
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
        let scroll = self.moment.read().expect("moment poisoned").scroll.0;
        let mut skipped = 0usize;

        for slot in stream.slots().iter().rev() {
            let block = slot.block();
            let mut lines = if pres.is_folded(block.kind()) {
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

        let layout = self.layout.read().expect("layout poisoned").clone();
        let modules = self.modules.clone();
        let pruned = layout.prune(&|id| modules.has_view(id));

        for (region, rect) in pruned.layout(Rect::sized(w, h)) {
            if rect.is_empty() {
                continue;
            }
            match region {
                Region::Stream => {
                    frame.place("stream", rect, self.stream_lines(rect));
                }
                Region::View(id) => {
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
pub fn default_layout() -> Region {
    use crate::region::{Constraint, Dir};
    Region::split(
        Dir::Vertical,
        Constraint::Cells(1),
        Region::view(crate::modules::status::ID),
        Region::split(
            Dir::Vertical,
            Constraint::Fill,
            Region::Stream,
            Region::view(crate::modules::input::ID),
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
        let rows = h.compose((80, 24)).rows();
        let last_content = rows
            .iter()
            .rev()
            .skip(2) // the prompt and its hint
            .find(|r| !r.trim().is_empty())
            .unwrap();
        assert!(
            last_content.contains("Cancelled") || last_content.contains("now break it"),
            "bottom-anchored, newest last: {last_content:?}"
        );
    }

    #[test]
    fn reasoning_is_folded_by_default_and_expands_without_changing_content() {
        let h = fed();
        let folded = h.compose((80, 40)).rows().join("\n");
        assert!(folded.contains("thought for"), "folded to a summary");
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
        *h.layout.write().unwrap() = Region::split(
            crate::region::Dir::Vertical,
            crate::region::Constraint::Cells(1),
            Region::view("mascot"),
            default_layout(),
        );
        assert!(
            h.compose((80, 24)).part("mascot").is_some(),
            "the registry is read fresh, not snapshotted"
        );
    }
}
