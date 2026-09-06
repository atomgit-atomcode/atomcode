//! The host: the four things no module may own.
//!
//! It holds the surface, the layout, focus, and the redraw cycle — and it knows
//! **no module by name**. Composition is: walk the region tree for rects, look
//! each leaf up in the registry, ask it to draw into its rect, check nothing
//! spilled. That last step is the pixel-level verdict on spatial
//! composability, and it runs in every frame in debug builds.

use std::sync::{Arc, Mutex, RwLock};

use atomcode_harness::session::SessionEvent;

use crate::block::{BlockId, Slot, Stream};
use crate::frame::{Frame, Line, Rect};
use crate::module::{Height, Modules};
use crate::moment::Moment;
use crate::region::Region;

/// Which blocks are shown how. Kept here, keyed by id, rather than on the block
/// — which is what makes "folding does not change content" structural.
#[derive(Default)]
pub struct Presentation {
    folded_kinds: Vec<&'static str>,
    /// Blocks folded or unfolded by hand, overriding the default for their
    /// kind.
    ///
    /// Per block, because a click is about *this* tool call. Folding every tool
    /// call in the transcript because one was clicked is a different gesture,
    /// and it already has a key (ctrl-t) — a click that did it would be a click
    /// that changed six other things the person was looking at.
    by_block: std::collections::HashMap<crate::block::BlockId, bool>,
}

impl Presentation {
    /// Kinds shown as one line unless expanded. Reasoning and tool calls are
    /// folded by default because a transcript is read for the answer, not for
    /// the working.
    pub fn default_folds() -> Self {
        Self {
            folded_kinds: vec!["reasoning", "tool_call"],
            by_block: std::collections::HashMap::new(),
        }
    }
    pub fn is_folded(&self, kind: &str) -> bool {
        self.folded_kinds.contains(&kind)
    }

    /// Whether one block is folded: what the person said about it, or failing
    /// that what its kind says.
    pub fn is_block_folded(&self, id: crate::block::BlockId, kind: &str) -> bool {
        match self.by_block.get(&id) {
            Some(folded) => *folded,
            None => self.is_folded(kind),
        }
    }

    /// Fold or unfold every block of a kind. The keyboard gesture.
    ///
    /// Per-block choices are dropped, because otherwise "unfold everything"
    /// would visibly not unfold everything.
    pub fn toggle(&mut self, kind: &'static str) {
        match self.folded_kinds.iter().position(|k| *k == kind) {
            Some(i) => {
                self.folded_kinds.remove(i);
            }
            None => self.folded_kinds.push(kind),
        }
        self.by_block.clear();
    }

    /// Fold or unfold one block. The pointing gesture.
    pub fn toggle_block(&mut self, id: crate::block::BlockId, kind: &str) {
        let folded = self.is_block_folded(id, kind);
        self.by_block.insert(id, !folded);
    }
}

/// What a click can fold.
///
/// Deliberately narrow. Everything the model *says* is what the transcript is
/// for, so making prose and reasoning click targets means most of the screen
/// silently swallows a click and folds something the person was reading. A tool
/// call is the one block that is genuinely a lid over a detail.
const CLICKABLE: [&str; 1] = ["tool_call"];

/// Which block each row of the stream came from, and where the stream was.
///
/// Composing is where this is known and clicking is where it is needed, so the
/// frame leaves it behind. A click is answered from the picture that was
/// actually on screen rather than from a second, re-derived one — the two would
/// drift the moment anything scrolled between the paint and the press.
#[derive(Default)]
pub struct Hits {
    rect: Rect,
    /// One entry per row of `rect`, top to bottom.
    rows: Vec<Option<(crate::block::BlockId, &'static str)>>,
    /// Where the "back to the bottom" badge was, when it was up.
    jump: Option<Rect>,
}

impl Hits {
    /// The block under a point, if the point is on one.
    pub fn at(&self, x: u16, y: u16) -> Option<(crate::block::BlockId, &'static str)> {
        if !self.rect.contains(x, y) {
            return None;
        }
        *self.rows.get((y - self.rect.y) as usize)?
    }

    /// Whether the point is on the badge. Checked first: it sits on top of a
    /// row of the stream, and the thing on top is the thing that was clicked.
    pub fn on_jump(&self, x: u16, y: u16) -> bool {
        self.jump.is_some_and(|r| r.contains(x, y))
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
    /// What was on screen last, so a click can be answered from it.
    hits: Mutex<Hits>,
    /// The width the stream was last laid out at. New content arrives between
    /// frames and has to be measured in the same terms the frame used.
    last_width: Mutex<u16>,
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
            hits: Mutex::new(Hits::default()),
            last_width: Mutex::new(0),
        }
    }

    /// Deliver one committed fact to every module.
    ///
    /// Producers first, then views: a view that reacts to the same fact should
    /// see a screen whose stream already contains it.
    pub fn absorb(&self, fact: &SessionEvent) {
        // Pinned reading. The scroll offset is measured from the bottom of the
        // conversation, and the model puts new output *at* that bottom — so a
        // reader who has scrolled up to study something would watch it slide
        // away by exactly as much as arrived, which is the screen refusing to
        // hold still. Growing the offset by what grew keeps the same lines
        // under the same eyes.
        //
        // Only while held back: at the bottom the whole point is to follow, and
        // measuring is O(the conversation), so it is not paid for nothing.
        let width = *self.last_width.lock().expect("width poisoned");
        let held = width > 0 && self.moment.read().expect("moment poisoned").scroll.0 > 0;
        let before = if held { self.stream_height(width) } else { 0 };

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

        if held {
            let grew = self.stream_height(width).saturating_sub(before);
            if grew > 0 {
                let mut m = self.moment.write().expect("moment poisoned");
                m.scroll = crate::moment::ScrollPos(m.scroll.0 + grew);
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
    fn stream_lines(&self, rect: Rect) -> (Vec<Line>, Vec<Option<(BlockId, &'static str)>>) {
        let stream = self.stream.read().expect("stream poisoned");
        let pres = self.presentation.read().expect("presentation poisoned");
        let mut out: Vec<Line> = Vec::new();
        // Grown in lockstep with `out`, so a row and its owner cannot get out
        // of step — the alternative is two loops that agree until one changes.
        let mut owner: Vec<Option<(BlockId, &'static str)>> = Vec::new();
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
                    owner.push(None); // a question is not a block yet
                }
            }
        }
        let scroll = self.moment.read().expect("moment poisoned").scroll.0;
        let mut skipped = 0usize;
        let _ = &skipped;

        for slot in stream.slots().iter().rev() {
            let block = slot.block();
            // Two different questions. Reasoning folds — that is what ctrl-r
            // is — but it is not a click target, because it is something the
            // model said and most of the screen is things the model said.
            let foldable = !block.content.always_open();
            let clickable = foldable && CLICKABLE.contains(&block.kind());
            let mut lines = if foldable && pres.is_block_folded(block.id, block.kind()) {
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
                owner.push(clickable.then_some((block.id, block.kind())));
            }
            if out.len() >= want {
                break;
            }
        }
        out.reverse();
        owner.reverse();
        // Push the content to the bottom of the rect when there is not enough
        // of it, so the newest line is always where the eye expects it.
        let pad = want.saturating_sub(out.len());
        let mut padded = vec![Line::empty(); pad];
        padded.extend(out);
        let mut owners = vec![None; pad];
        owners.extend(owner);
        (padded, owners)
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
        let asked = |id: &str| -> u16 { asked_height(&modules, id, &moment, w) };
        let mut stream_rect: Option<Rect> = None;
        for (region, rect) in pruned.layout_with(Rect::sized(w, h), &asked) {
            if rect.is_empty() {
                continue;
            }
            match region {
                Region::Stream => {
                    let (lines, owners) = self.stream_lines(rect);
                    *self.hits.lock().expect("hits poisoned") = Hits {
                        rect,
                        rows: owners,
                        jump: None,
                    };
                    *self.last_width.lock().expect("width poisoned") = rect.w;
                    stream_rect = Some(rect);
                    frame.place("stream", rect, lines);
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
                    let cap = match view.height(&moment, rect.w) {
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

        // Held back, so the conversation has moved on below the fold. Say how
        // far, and make saying so the way back — a person who has scrolled up
        // should not have to know that ctrl-e or End exists.
        //
        // Drawn *over* the stream's last row rather than inside it, so the
        // badge costs no line of content: later parts win the cells they cover,
        // and only those.
        if let (Some(rect), true) = (stream_rect, moment.scroll.0 > 0) {
            let caps = moment.caps;
            let label = format!(
                " {} 还有 {} 行 · 点击回到底部 ",
                caps.g(crate::caps::Glyph::Down),
                moment.scroll.0
            );
            let want = crate::width::str_width(&label);
            if rect.h > 0 && want <= rect.w as usize {
                let badge = Rect::new(
                    rect.x + rect.w - want as u16,
                    rect.bottom() - 1,
                    want as u16,
                    1,
                );
                let style = crate::theme::bg(crate::theme::Role::PanelBg)
                    .under(crate::theme::fg(crate::theme::Role::PanelFg));
                frame.place("jump-to-bottom", badge, vec![Line::styled(label, style)]);
                self.hits.lock().expect("hits poisoned").jump = Some(badge);
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

        // Last, over everything, including a modal: the selection is a
        // rectangle on the screen, and the screen is what was pointed at.
        if let Some(sel) = moment.selection {
            frame.highlight(&sel);
        }

        debug_assert!(
            frame.containment_violations().is_empty(),
            "a module drew outside its rect: {:?}",
            frame.containment_violations()
        );
        *self.painted.lock().expect("counter poisoned") += 1;
        frame
    }

    /// How many rows the conversation gets on a screen this size.
    ///
    /// The other half of a scroll bound. Scrolling back is limited by how much
    /// there is to read *minus what is already on screen*: without the second
    /// term the stream scrolls off its own top and the region goes blank, which
    /// reads as the conversation having been lost.
    pub fn stream_rows(&self, size: (u16, u16), moment: &Moment) -> u16 {
        let (w, h) = size;
        let modules = self.modules.clone();
        let pruned = self.layout.tree().prune(&|id| modules.has_view(id));
        let asked = |id: &str| -> u16 { asked_height(&modules, id, moment, w) };
        pruned
            .layout_with(Rect::sized(w, h), &asked)
            .into_iter()
            .find_map(|(region, rect)| matches!(region, Region::Stream).then_some(rect.h))
            .unwrap_or(h)
    }

    /// How far back the stream can be scrolled, in rendered lines. Zero when
    /// everything there is to read is already on screen.
    /// The moment is an argument, not a field read: the caller is holding the
    /// write lock on it when it asks, and a `RwLock` does not forgive that.
    pub fn scroll_limit(&self, size: (u16, u16), moment: &Moment) -> usize {
        self.stream_height(size.0)
            .saturating_sub(self.stream_rows(size, moment) as usize)
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
                if !b.content.always_open() && pres.is_block_folded(b.id, b.kind()) {
                    1
                } else {
                    b.content.lines(width).len()
                }
            })
            .sum()
    }

    /// The block under a point on the last painted frame, if it is one that
    /// can be folded.
    pub fn block_at(&self, x: u16, y: u16) -> Option<(BlockId, &'static str)> {
        self.hits.lock().expect("hits poisoned").at(x, y)
    }

    /// Whether a point is on the "back to the bottom" badge.
    pub fn jump_at(&self, x: u16, y: u16) -> bool {
        self.hits.lock().expect("hits poisoned").on_jump(x, y)
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

/// What one module asks for, vertically. Shared by `compose` and
/// [`Host::stream_rows`] so the rows a scroll is measured against are the same
/// rows that get painted.
fn asked_height(modules: &Modules, id: &str, moment: &Moment, width: u16) -> u16 {
    modules
        .view(id)
        .map(|v| match v.height(moment, width) {
            Height::Fixed(n) | Height::Hug(n) => n,
            Height::Fill => 1,
        })
        .unwrap_or(1)
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
    fn a_click_lands_on_the_block_that_was_painted_there() {
        // The click is answered from the frame that was actually on screen, not
        // from a re-derived one: the two would drift the moment anything
        // scrolled between the paint and the press.
        let h = fed();
        let size = (80, 24);
        let frame = h.compose(size);
        let stream = frame.part("stream").expect("a conversation");

        // Every row that shows a foldable block answers, and answers with that
        // block; the chrome around it answers with nothing.
        let hit_rows: Vec<u16> = (stream.rect.y..stream.rect.bottom())
            .filter(|&y| h.block_at(2, y).is_some())
            .collect();
        assert!(!hit_rows.is_empty(), "no foldable block on screen");
        assert_eq!(h.block_at(2, stream.rect.bottom() + 1), None, "the prompt");

        // Clicking one folds exactly it, and clicking again gives it back.
        // A tool call, specifically: a one-line reasoning block looks the same
        // folded as open, so it would prove nothing either way.
        let (id, kind) = hit_rows
            .iter()
            .filter_map(|&y| h.block_at(2, y))
            .find(|(_, kind)| *kind == "tool_call")
            .expect("a tool call on screen");
        let before = h.compose(size).rows().join("\n");
        h.presentation.write().unwrap().toggle_block(id, kind);
        let folded = h.compose(size).rows().join("\n");
        assert_ne!(before, folded, "clicking a block changed nothing");
        h.presentation.write().unwrap().toggle_block(id, kind);
        assert_eq!(h.compose(size).rows().join("\n"), before, "not an inverse");
    }

    #[test]
    fn only_a_tool_call_answers_a_click() {
        // Everything the model says is what the transcript is *for*. Making
        // prose and reasoning click targets meant most of the screen silently
        // swallowed a click and folded away what the person was reading.
        let h = fed();
        let size = (80, 40);
        let frame = h.compose(size);
        let rect = frame.part("stream").unwrap().rect;
        let kinds: Vec<&str> = (rect.y..rect.bottom())
            .filter_map(|y| h.block_at(2, y))
            .map(|(_, kind)| kind)
            .collect();
        assert!(!kinds.is_empty(), "nothing is clickable at all");
        assert!(
            kinds.iter().all(|k| *k == "tool_call"),
            "these answer a click too: {kinds:?}"
        );
    }

    #[test]
    fn reasoning_still_folds_by_key_even_though_it_is_not_a_click_target() {
        // The two questions are different, and collapsing them into one broke
        // ctrl-r: a block that cannot be clicked must still be foldable.
        let h = fed();
        let open = |folded: bool| {
            let mut p = h.presentation.write().unwrap();
            if p.is_folded("reasoning") != folded {
                p.toggle("reasoning");
            }
        };
        open(true);
        let short = h.compose((80, 40)).rows().join("\n");
        open(false);
        let long = h.compose((80, 40)).rows().join("\n");
        assert_ne!(short, long, "reasoning stopped folding");
    }

    #[test]
    fn new_output_does_not_slide_the_view_out_from_under_a_reader() {
        // The complaint this answers: while the model is producing, the reader
        // scrolls up to study something and it walks off the top, because the
        // offset is measured from a bottom that keeps moving.
        let h = fed();
        let size = (80, 16);
        let _ = h.compose(size); // the width has to be known to measure growth
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(4);
        // The stream's own lines, not the flattened screen: the badge sits on
        // top of them and its count is *supposed* to move as output arrives.
        let read = |f: &crate::frame::Frame| {
            f.part("stream")
                .unwrap()
                .lines
                .iter()
                .map(|l| l.plain())
                .collect::<Vec<_>>()
        };
        let held = read(&h.compose(size));

        for n in 0..3 {
            h.absorb(&SessionEvent::AssistantChunk {
                delta: format!("more output, line {n}\n"),
                reasoning: false,
                turn: 9,
                round: 0,
            });
        }
        assert_eq!(
            read(&h.compose(size)),
            held,
            "the screen moved while it was being read"
        );

        // And following resumes the moment the reader asks for it.
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos::BOTTOM;
        let following = read(&h.compose(size));
        assert_ne!(following, held);
        assert!(
            following.iter().any(|l| l.contains("line 2")),
            "the newest output is on screen: {following:?}"
        );
    }

    #[test]
    fn being_held_back_says_so_and_the_badge_is_the_way_out() {
        let h = fed();
        let size = (80, 16);
        let _ = h.compose(size);
        assert!(
            h.compose(size).part("jump-to-bottom").is_none(),
            "nothing to say while following"
        );

        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(7);
        let frame = h.compose(size);
        let badge = frame.part("jump-to-bottom").expect("no badge");
        let text = badge.lines[0].plain();
        assert!(text.contains('7'), "it says how far behind: {text:?}");

        // It sits on the stream's last row, and it is what a click there hits.
        let stream = frame.part("stream").unwrap();
        assert_eq!(badge.rect.bottom(), stream.rect.bottom());
        assert!(h.jump_at(badge.rect.x, badge.rect.y));
        assert!(!h.jump_at(badge.rect.x.saturating_sub(1), badge.rect.y));
        assert!(frame.containment_violations().is_empty());
    }

    #[test]
    fn folding_one_block_leaves_its_siblings_alone() {
        // The difference between the pointing gesture and the keyboard one. A
        // click that folded every tool call would change six other things the
        // person was looking at.
        let h = fed();
        let size = (80, 40);
        let _ = h.compose(size);
        let ids: Vec<_> = {
            let stream = h.stream.read().unwrap();
            stream
                .slots()
                .iter()
                .map(|s| (s.block().id, s.block().kind()))
                .filter(|(_, k)| *k == "tool_call")
                .collect()
        };
        assert!(ids.len() >= 2, "need two tool calls to tell them apart");
        let pres = || {
            h.presentation
                .read()
                .unwrap()
                .is_block_folded(ids[1].0, ids[1].1)
        };
        let other = pres();
        h.presentation
            .write()
            .unwrap()
            .toggle_block(ids[0].0, ids[0].1);
        assert_eq!(pres(), other, "the sibling moved too");
    }

    #[test]
    fn the_stream_gets_the_rows_the_chrome_does_not() {
        let h = fed();
        let m = Moment::default();
        let rows = h.stream_rows((80, 24), &m);
        assert!(
            rows > 0 && rows < 24,
            "status and prompt take their share: {rows}"
        );
        assert_eq!(
            h.compose((80, 24)).part("stream").unwrap().rect.h,
            rows,
            "the rows a scroll is measured against are the rows that get painted"
        );
    }

    #[test]
    fn scrolling_stops_at_the_oldest_line_not_past_it() {
        let h = fed();
        // A screen tall enough to hold the whole conversation has nowhere to go.
        let m = Moment::default();
        assert_eq!(h.scroll_limit((80, 200), &m), 0, "nothing above the fold");

        // A short one can go back exactly as far as there is more to read.
        let size = (80, 12);
        let limit = h.scroll_limit(size, &m);
        assert!(limit > 0, "the conversation does not fit in 12 rows");
        assert_eq!(
            limit,
            h.stream_height(80) - h.stream_rows(size, &m) as usize,
            "what there is to read, minus what is already on screen"
        );

        // At the limit the oldest line is on screen. One line further used to
        // be reachable, and it emptied the region.
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
        let text = h.compose(size).rows().join("\n");
        assert!(
            !text.trim().is_empty(),
            "scrolled to the top, not into nothing"
        );
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
