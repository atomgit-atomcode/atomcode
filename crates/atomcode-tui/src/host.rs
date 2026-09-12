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

/// How a kind of block is shown.
///
/// Three states, not two. `Folded` is a lid: one row standing in for the whole
/// block, which is the only "off" there used to be. `Hidden` draws nothing at
/// all — the block is still in the stream, still in the content hashes, still
/// saying exactly what it said; it is simply not on the screen.
///
/// The difference is a row. A reader who is not reading the working does not
/// want a `◐ 思考 7 行` lid between every call telling them how much working
/// there is that they are not reading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Showing {
    /// Drawn in full.
    #[default]
    Open,
    /// One row standing in for the whole block.
    Folded,
    /// Not drawn at all.
    Hidden,
}

/// The kinds a reader may take off the screen entirely.
///
/// Reasoning, and nothing else. It is the working rather than the answer — the
/// one thing the model produces that a person may reasonably want gone once
/// they have read it — and it opens hidden for that reason. Everything else on
/// the stream is content: what was asked, what was answered, what a tool
/// returned. Putting a lid over those is a different decision, and `ctrl-t`
/// already makes it.
///
/// A list rather than a flag on the block, for the same reason [`CLICKABLE`] is
/// one: it is about what the screen does, not about what the block is.
const HIDEABLE: [&str; 1] = ["reasoning"];

/// Which blocks are shown how. Kept here, keyed by kind, rather than on the
/// block — which is what makes "folding does not change content" structural.
#[derive(Default)]
pub struct Presentation {
    /// How each kind is shown right now. A kind nobody has touched is `Open`.
    by_kind: Vec<(&'static str, Showing)>,
    /// Blocks folded or unfolded by hand, overriding the default for their
    /// kind.
    ///
    /// Per block, because a click is about *this* tool call. Folding every tool
    /// call in the transcript because one was clicked is a different gesture,
    /// and it already has a key (ctrl-t) — a click that did it would be a click
    /// that changed six other things the person was looking at.
    by_block: std::collections::HashMap<BlockId, bool>,
}

impl Presentation {
    /// How the screen opens: reasoning away, tool calls behind a lid.
    ///
    /// Both are the working rather than the answer, and a transcript is read for
    /// the answer. Tool calls keep a lid where reasoning does not, because
    /// *what was run* is part of that answer: someone glancing at a transcript
    /// wants to know that a file was read, not what the model was thinking
    /// while it read it.
    pub fn default_folds() -> Self {
        Self {
            by_kind: vec![
                ("reasoning", Showing::Hidden),
                ("tool_call", Showing::Folded),
            ],
            by_block: std::collections::HashMap::new(),
        }
    }

    /// How this kind is shown now.
    pub fn showing(&self, kind: &str) -> Showing {
        self.by_kind
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, s)| *s)
            .unwrap_or_default()
    }

    fn set(&mut self, kind: &'static str, to: Showing) {
        match self.by_kind.iter_mut().find(|(k, _)| *k == kind) {
            Some(entry) => entry.1 = to,
            None => self.by_kind.push((kind, to)),
        }
    }

    pub fn is_folded(&self, kind: &str) -> bool {
        self.showing(kind) == Showing::Folded
    }

    /// Whether this kind draws nothing at all.
    pub fn is_hidden(&self, kind: &str) -> bool {
        self.showing(kind) == Showing::Hidden
    }

    /// Whether one block is folded: what the person said about it, or failing
    /// that what its kind says.
    ///
    /// Hiding is not asked here. A block that draws nothing has no row to fold,
    /// and both the painter and the height check [`is_hidden`] before they get
    /// this far.
    ///
    /// [`is_hidden`]: Self::is_hidden
    pub fn is_block_folded(&self, id: BlockId, kind: &str) -> bool {
        match self.by_block.get(&id) {
            Some(folded) => *folded,
            None => self.is_folded(kind),
        }
    }

    /// Show more of every block of a kind, or put it away again. The keyboard
    /// gesture and the slash command, one implementation called once.
    ///
    /// The cycle is `Hidden → Folded → Open → Hidden` for a kind that may be
    /// hidden, and `Folded → Open → Folded` for one that may not: each press
    /// reveals one step more and the last puts it back where it started. That
    /// order is what makes a press on reasoning mean "show me" — the first
    /// thing it hands over is the lid, which says how much there is before
    /// anybody spends a screenful reading it.
    ///
    /// Per-block choices are dropped, because otherwise "unfold everything"
    /// would visibly not unfold everything.
    pub fn toggle(&mut self, kind: &'static str) {
        let next = match self.showing(kind) {
            Showing::Hidden => Showing::Folded,
            Showing::Folded => Showing::Open,
            Showing::Open if HIDEABLE.contains(&kind) => Showing::Hidden,
            Showing::Open => Showing::Folded,
        };
        self.set(kind, next);
        self.by_block.clear();
    }

    /// Fold or unfold one block. The pointing gesture.
    pub fn toggle_block(&mut self, id: BlockId, kind: &str) {
        let folded = self.is_block_folded(id, kind);
        self.by_block.insert(id, !folded);
    }
}

/// What a click can fold.
///
/// Narrow, for two different reasons — which is why this is a list and not a
/// predicate over "is it foldable".
///
/// Prose is out. Everything the model *says* is what the transcript is for, and
/// it is the largest surface on the screen, so a click that folded the answer
/// someone was reading away is the one gesture that can lose work. A click on
/// prose stays a no-op.
///
/// A thought and a tool call are in. Both are drawn as a one-line lid over a
/// detail — `· 思考 3 行`, `● read_file(a.rs) · ok` — and a click on a lid has
/// exactly one meaning. Folding a thought was reachable only by ctrl-r, which
/// moves *every* thought in the transcript; the row itself answered nothing, so
/// pointing at a lid opens that lid and costs no other gesture. A press that
/// moves is still a selection, and ctrl-r still does them all at once.
const CLICKABLE: [&str; 2] = ["tool_call", "reasoning"];

/// Whether a blank row goes between two blocks stacked next to each other.
///
/// A tool call is its own paragraph. Without a row between them the model's
/// prose runs straight into the `●` header and the two read as one wall — the
/// sentence and the thing that was run at the same level. One blank row is the
/// whole of the fix, and it goes on *both* sides of a call, because
/// `⎿ ok · 12 行` followed by the next sentence has the same problem the other
/// way round.
///
/// Nothing goes between two tool calls. A run of them is one thought, and a
/// screen of six calls separated by five gaps is a screen that no longer shows
/// what was done in one glance.
///
/// One definition, consulted by both the painter and the height the scroll is
/// measured against — the two have to agree or the last rows of a long
/// transcript become unreachable.
fn blank_between(a: &str, b: &str) -> bool {
    (a == "tool_call") != (b == "tool_call")
}

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
    /// Where the composer was drawn, so a click in it can find a caret.
    field: Option<Rect>,
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
        // When a turn opened or closed, on the clock the host was handed. Here
        // rather than in a module because this is the one place that sees both
        // the fact and the reading — and a duration on screen is the difference
        // of two readings the log does not carry (docs/adr/0008).
        match fact {
            SessionEvent::TurnStart { .. } => {
                let mut m = self.moment.write().expect("moment poisoned");
                let now = m.now;
                m.turn_started = Some(now);
            }
            SessionEvent::TurnEnd { .. } => {
                self.moment.write().expect("moment poisoned").turn_started = None;
            }
            _ => {}
        }

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

        // What the person said is what an up-arrow goes back through. Folded
        // here because this is where facts land, and consecutive repeats are
        // dropped the way every shell drops them.
        if let SessionEvent::UserMessage { text, .. } = fact {
            let mut m = self.moment.write().expect("moment poisoned");
            if !text.trim().is_empty()
                && m.history.last().map(String::as_str) != Some(text.as_str())
            {
                m.history.push(text.clone());
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
        //
        // Unless a row is drawing it as a modal: then the modal is where it is
        // being answered, and a second copy here would be the same question
        // asked twice on one screen.
        if let Some((_, question)) = self.asks.peek().filter(|_| !self.overlays.is_open()) {
            let pending = crate::content::ChoiceBlock {
                question: crate::ask::recorded(&question),
                options: question
                    .options
                    .iter()
                    .map(|a| crate::ask::answer_label(&a.value, &a.label))
                    .collect(),
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

        // The kind of the block whose rows went in last, which — iterating
        // newest-first — is the one *below* the block being rendered now. Only
        // blocks that drew something count: a block that renders no rows is not
        // a neighbour, whatever its kind says.
        let mut below: Option<&'static str> = None;

        for slot in stream.slots().iter().rev() {
            let block = slot.block();
            let kind = block.kind();
            // A kind the reader has taken off the screen draws nothing at all —
            // not even the blank row, because it is not a neighbour of anything
            // either. Asked before the `below` bookkeeping, so a hidden block
            // leaves the seam between its visible neighbours the way it was.
            if pres.is_hidden(kind) {
                continue;
            }
            // Two different questions. Reasoning and tool calls both fold — that
            // is what ctrl-r and ctrl-t are — but what a click may fold is
            // narrower than what folds: prose is out, because it is what the
            // transcript is for and most of the screen is things the model said.
            let foldable = !block.content.always_open();
            let clickable = foldable && CLICKABLE.contains(&kind);
            let mut lines = if foldable && pres.is_block_folded(block.id, kind) {
                vec![block.content.summary(rect.w)]
            } else {
                // The one place a block is rendered for the screen. Its row count
                // comes first, because a settled block already knows it: a block
                // that lies entirely above the reader is then skipped in
                // arithmetic instead of being rendered and thrown away, which is
                // what makes scrolling back through a long conversation cost the
                // screen rather than the session.
                let (n, rendered) = slot.rows_at(rect.w);
                // Not a neighbour if it draws nothing — same as the draw path
                // below, which leaves `below` alone for an empty block.
                if n == 0 {
                    continue;
                }
                // A question in flight can already have filled the screen above
                // this point, in which case nothing below it can be drawn.
                if out.len() >= want {
                    break;
                }
                // `<=` because a block whose last row is the first off-screen one
                // is wholly above the reader. `skipped` never passes `scroll`:
                // each pass adds n and the guard already proved it fits.
                if n <= scroll.saturating_sub(skipped) {
                    skipped += n;
                    if below.is_some_and(|b| blank_between(b, kind)) {
                        skipped += 1;
                    }
                    below = Some(kind);
                    continue;
                }
                // A hit hands back no lines because it did not render any; this
                // is the block being looked at, so it gets rendered now.
                match rendered {
                    Some(lines) => lines,
                    None => block.content.lines(rect.w),
                }
            };
            if lines.is_empty() {
                continue;
            }
            let blank = below.is_some_and(|b| blank_between(b, kind));
            below = Some(kind);
            // The blank belongs to the seam between this block and the one
            // below it, so it goes into the buffer first: rows go in
            // bottom-to-top, and a row pushed before this block's own rows ends
            // up between the two. It belongs to nobody — a click there folds
            // nothing.
            if blank && out.len() < want {
                if skipped < scroll {
                    skipped += 1;
                } else {
                    out.push(Line::empty());
                    owner.push(None);
                }
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
                owner.push(clickable.then_some((block.id, kind)));
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
                        field: None,
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
                        self.hits.lock().expect("hits poisoned").field = Some(rect);
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
            let rect = crate::overlay::modal_rect(Rect::sized(w, h), modal.size(), modal.rows());
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
    ///
    /// Every row the painter draws is counted here, and no others: the blanks
    /// between blocks, and — the same question asked the same way — a kind the
    /// reader has hidden, which the painter skips and this therefore skips too.
    /// Get one wrong in either direction and the scroll is measured against a
    /// screen that never existed: count a row nobody drew and the last few rows
    /// of a long transcript can never be scrolled to, because the limit is this
    /// number minus the height of the window; miss one that was drawn and the
    /// top goes out of reach the same way.
    ///
    /// [`stream_lines`]: Self::stream_lines
    ///
    /// The row count of a settled block is remembered on the block itself, so
    /// this walks the whole conversation in arithmetic rather than in markdown.
    /// It is called per chunk while the reader is scrolled back, which is the
    /// one time the whole conversation is in the sum.
    pub fn stream_height(&self, width: u16) -> usize {
        let stream = self.stream.read().expect("stream poisoned");
        let pres = self.presentation.read().expect("presentation poisoned");
        let mut total = 0usize;
        let mut below: Option<&'static str> = None;
        for slot in stream.slots() {
            let b = slot.block();
            // What is not drawn is not a row of the transcript — and by the same
            // token it is not a neighbour, so the blanks around it stay put.
            if pres.is_hidden(b.kind()) {
                continue;
            }
            let n = if !b.content.always_open() && pres.is_block_folded(b.id, b.kind()) {
                1
            } else {
                slot.rows_at(width).0
            };
            if n == 0 {
                continue;
            }
            if below.is_some_and(|k| blank_between(k, b.kind())) {
                total += 1;
            }
            below = Some(b.kind());
            total += n;
        }
        total
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

    /// Where the composer was drawn on the last frame, if it was.
    pub fn field_rect(&self) -> Option<Rect> {
        self.hits.lock().expect("hits poisoned").field
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

/// The composer: the live line and the reserved tip row above the field.
///
/// One definition, because every arrangement that has an input has this above
/// it — the input box is where a person's eyes are, and the one thing that
/// answers "is it stuck?" belongs against it rather than in a panel beside the
/// conversation.
///
/// Written into the tree rather than claimed by the live line's own row the way
/// the mascot claims its strip: `LayoutOp::Show` has two sides, above the
/// conversation and below the status bar, and both are the wrong side of the
/// input box.
///
/// The live line is `Hug`-ish: it asks for no rows between turns, so the
/// composer closes up around the field instead of standing on a blank row. The
/// tip row beneath it is the deliberate exception — it asks for its row always,
/// which is why a tip can never move the box out from under a hand reaching for
/// it. See `modules::tip`.
///
/// The blank row above the live line is the line's own for that same reason: a
/// `gap` on this flex is counted between the children whether or not the line is
/// mounted, so the margin would outlive the thing it spaces out. It is above
/// only — under the line sits the reserved row, which is not a margin and is
/// drawn whether or not there is a line to space off.
pub fn composer() -> Region {
    use crate::el::Item;
    use crate::region::Dir;
    Region::flex(
        Dir::Vertical,
        vec![
            Item::hug(Region::view(crate::modules::live::ID)),
            Item::hug(Region::view(crate::modules::tip::ID)),
            Item::grow(Region::view(crate::modules::input::ID)),
        ],
    )
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
            composer(),
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

    /// Content that counts how many times it was asked to render.
    ///
    /// The claim under test is about a cost, and a cost needs an instrument:
    /// nothing else about a frame can tell a block that was rendered from one
    /// that was skipped in arithmetic.
    #[derive(Debug)]
    struct Counted {
        lines: Vec<String>,
        asked: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::block::Content for Counted {
        fn kind(&self) -> &'static str {
            "assistant"
        }
        fn content_hash(&self) -> crate::block::ContentHash {
            crate::block::hash_of(&self.lines.iter().map(|l| l.as_str()).collect::<Vec<_>>())
        }
        fn lines(&self, _w: u16) -> Vec<crate::frame::Line> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.lines
                .iter()
                .map(|l| crate::frame::Line::raw(l.as_str()))
                .collect()
        }
    }

    /// A host holding `blocks` settled prose blocks, plus the counter they
    /// report to. Three rows each, so the arithmetic in the test is easy to
    /// follow against a twenty-row screen.
    fn counted(blocks: usize) -> (Host, Arc<std::sync::atomic::AtomicUsize>) {
        let h = host();
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut s = h.stream.write().unwrap();
        let mut w = s.writer("bench");
        for i in 0..blocks {
            w.emit(
                crate::block::Coord::default(),
                Arc::new(Counted {
                    lines: vec![
                        format!("paragraph {i}"),
                        "second row".to_string(),
                        "third row".to_string(),
                    ],
                    asked: asked.clone(),
                }),
            );
        }
        drop(w);
        drop(s);
        (h, asked)
    }

    #[test]
    fn scrolling_back_renders_the_screen_and_not_the_session() {
        // 「翻上去特别卡」. The painter walks newest-first and skips `scroll` rows
        // to reach the viewport, so reaching it used to mean rendering every
        // block newer than the reader — the whole session, per frame. A settled
        // block knows its row count, so what is above the fold is now skipped in
        // arithmetic.
        //
        // The bar is that one screenful costs the same in a long session as in a
        // short one. A bound alone would pass for a cost that grows slowly.
        let cost_of_one_scrolled_frame = |blocks: usize| -> usize {
            let (h, asked) = counted(blocks);
            let size = (80, 20);
            let limit = h.scroll_limit(size, &h.moment.read().unwrap().clone());
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
            // Warm: the first paint after a jump to a new position measures what
            // it shows. Measure the steady state, which is what a wheel costs.
            let _ = h.compose(size);
            asked.store(0, std::sync::atomic::Ordering::SeqCst);
            let _ = h.compose(size);
            asked.load(std::sync::atomic::Ordering::SeqCst)
        };

        let short = cost_of_one_scrolled_frame(60);
        let long = cost_of_one_scrolled_frame(240);
        assert!(
            short <= 20 && long <= 20,
            "one screenful of a scrolled frame rendered {short} (60 blocks) and \
             {long} (240 blocks) blocks; the screen holds at most twenty rows"
        );
        assert_eq!(
            short, long,
            "a four-times-longer session made one scrolled frame cost more: what \
             is above the fold is still being rendered"
        );
    }

    #[test]
    fn a_settled_block_is_measured_once_per_width_not_once_per_call() {
        // The cheaper half of the same property, without scrolling: working out
        // how tall a settled block is is a measurement, not a question — and
        // `stream_height` is asked per chunk while the reader is scrolled back,
        // so a re-render here is the whole session re-rendered per chunk.
        //
        // Once per block is the most that can be owed: the first walk has to
        // measure what the screen never asked for, and nothing after it does.
        // Whether the block is still *drawn* every frame is a different
        // question, and not this one.
        let (h, asked) = counted(20);
        let _ = h.compose((80, 20));
        let orders = std::sync::atomic::Ordering::SeqCst;

        asked.store(0, orders);
        let first = h.stream_height(80);
        assert!(first > 0);
        let measured = asked.load(orders);
        assert!(
            measured <= 20,
            "the first walk measured {measured} blocks for a 20-block session"
        );

        asked.store(0, orders);
        for _ in 0..5 {
            let _ = h.stream_height(80);
        }
        assert_eq!(
            asked.load(orders),
            0,
            "asking again how tall settled blocks are rendered them again"
        );
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
    fn a_tool_call_gets_a_row_of_its_own_above_and_below() {
        // 「太挤了」. Prose and the `●` header were adjacent rows, so the
        // sentence read as the tool's own caption and a screenful of both was
        // one wall of text. `看看那个目录` is the standing case: the model said
        // it and the very next row was the call it was asking for.
        let h = fed();
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        let prose = rows
            .iter()
            .position(|r| r.contains("看看那个目录"))
            .expect("the model's words are on screen");
        let call = rows
            .iter()
            .position(|r| r.contains("$("))
            .expect("the call it asked for");
        assert_eq!(
            call,
            prose + 2,
            "the prose and the call are not two paragraphs: {:?}",
            &rows[prose..=call]
        );
        assert!(
            rows[prose + 1].trim().is_empty(),
            "the one row between them is not blank: {:?}",
            &rows[prose..=call]
        );
    }

    #[test]
    fn two_calls_in_a_row_stay_one_stretch_of_work() {
        // The other half of the same decision: a run of tools is one thought,
        // and a gap between each of them would be a screen that no longer shows
        // in one glance what was done.
        let h = fed();
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        let first = rows
            .iter()
            .position(|r| r.contains("read_file(a.rs)"))
            .expect("the first call");
        let second = rows
            .iter()
            .position(|r| r.contains("read_file(b.rs)"))
            .expect("the second call");
        assert_eq!(
            second,
            first + 1,
            "a run of tools was broken up: {:?}",
            &rows[first..=second]
        );
    }

    #[test]
    fn the_blanks_are_rows_the_scroll_can_reach() {
        // A blank the painter drew and the height forgot is a row of the
        // transcript that scrolling can never arrive at — the top of a long
        // conversation would stop a few rows short.
        let h = fed();
        let size = (80, 12);
        let m = Moment::default();
        let limit = h.scroll_limit(size, &m);
        assert!(limit > 0, "the conversation does not fit in 12 rows");
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
        let rows: Vec<String> = h
            .compose(size)
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("fix the build")),
            "scrolled to the limit and the first thing said is not there: {rows:?}"
        );
    }

    #[test]
    fn the_live_line_stands_against_the_field_and_only_while_a_turn_runs() {
        use crate::modules::live;
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<live::Live>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<input::Input>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        let h = Host::new(mods, default_layout());

        // Between turns: no row at all, so the conversation keeps it. A line
        // standing there saying "空闲" would be chrome where the words go.
        let at_rest = h.compose((60, 12));
        assert!(at_rest.part("live").is_none(), "nothing to say, no row");
        let field_at_rest = at_rest.part("input").expect("the field").rect;

        // A turn opens, and the clock is already running: the reading the host
        // stamps the opening with is the one the line subtracts from.
        h.moment.write().unwrap().now = crate::moment::Timestamp::millis(8_000);
        h.absorb(&SessionEvent::TurnStart { turn: 1 });
        {
            let mut m = h.moment.write().unwrap();
            m.activity = crate::moment::Activity::Working;
            m.now = crate::moment::Timestamp::millis(21_000);
        }
        let running = h.compose((60, 12));
        let line = running.part("live").expect("the live line");
        let field = running.part("input").expect("the field").rect;
        assert_eq!(
            line.rect.y + line.rect.h,
            field.y,
            "against the field, not adrift in the screen"
        );
        // The composer is anchored to the bottom, so the rows it grows into come
        // off the conversation above it: the field does not move, and the stream
        // hands over the live line and the blank row above it — which is what
        // keeps it off the words above. Under the line there is nothing of this
        // module's to give back: the composer's reserved row is what separates
        // it from the field's rule, and it is there in either case.
        assert_eq!(field, field_at_rest, "the field does not move");
        assert_eq!(
            running.part("stream").expect("the conversation").rect.h,
            at_rest.part("stream").expect("the conversation").rect.h - 2,
            "and what it costs is the line and the row above it"
        );
        let said: String = line.lines.iter().map(|l| l.plain()).collect();
        assert!(said.contains("正在等待模型"), "{said}");
        assert!(
            said.contains("13s"),
            "stamped where facts land, drawn from what the host injected: {said}"
        );

        // `[[remove]] id = "tui-panel-live"`: the row is gone from the tree as
        // well as from the registry, and the composer closes up rather than
        // leaving the blank row it was reserving.
        h.modules.remove_view("live");
        let removed = h.compose((60, 12));
        assert!(removed.part("live").is_none());
        assert_eq!(
            removed.part("input").expect("the field").rect,
            field_at_rest,
            "a composer of one is the field"
        );
    }

    #[test]
    fn a_scrolled_up_reader_is_not_shown_the_live_line() {
        // The rows go back to the conversation rather than to the composer, and
        // the field below does not move: the line is the only thing that stops
        // being asked for.
        use crate::modules::live;
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<live::Live>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<input::Input>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        let h = Host::new(mods, default_layout());

        h.absorb(&SessionEvent::TurnStart { turn: 1 });
        {
            let mut m = h.moment.write().unwrap();
            m.activity = crate::moment::Activity::Working;
            m.now = crate::moment::Timestamp::millis(4_000);
        }
        let at_bottom = h.compose((60, 12));
        assert!(
            at_bottom.part("live").is_some(),
            "the line is up while the reader is at the bottom"
        );
        let field = at_bottom.part("input").expect("the field").rect;

        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(1);
        let scrolled = h.compose((60, 12));
        assert!(
            scrolled.part("live").is_none(),
            "reading history is not looking at the foot of the conversation"
        );
        assert_eq!(
            scrolled.part("input").expect("the field").rect,
            field,
            "the box does not move for it"
        );
        assert_eq!(
            scrolled.part("stream").expect("the conversation").rect.h,
            at_bottom.part("stream").expect("the conversation").rect.h + 2,
            "and the line and its blank row are the words' again"
        );
    }

    #[test]
    fn the_reserved_row_is_kept_and_a_tip_can_never_move_the_box() {
        use crate::modules::{live, tip};
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<live::Live>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<tip::Tip>::new())).unwrap();
        mods.add_view(Arc::new(Mounted::<input::Input>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        let h = Host::new(mods, default_layout());

        // The deliberate opposite of the live line: this row is there between
        // turns as well. That is what the module exists for — a tip that pushed
        // the box down as it appeared would move the field out from under a
        // hand already on its way to it.
        let idle = h.compose((60, 12));
        let tip_row = idle.part("tip").expect("the reserved row");
        let field = idle.part("input").expect("the field").rect;
        assert_eq!(tip_row.rect.h, 1, "one row, asked for unconditionally");
        assert_eq!(
            tip_row.rect.y + tip_row.rect.h,
            field.y,
            "directly above the field, not adrift"
        );
        assert!(
            tip_row.lines.iter().all(|l| l.plain().trim().is_empty()),
            "blank until something writes one: {:?}",
            tip_row.lines.iter().map(|l| l.plain()).collect::<Vec<_>>()
        );

        // A turn in flight takes its rows off the conversation above, not from
        // here: the live line and this row do not compete for the same row, and
        // the field sits still either way.
        h.absorb(&SessionEvent::TurnStart { turn: 1 });
        h.moment.write().unwrap().activity = crate::moment::Activity::Working;
        let running = h.compose((60, 12));
        assert!(running.part("live").is_some(), "the line is up this turn");
        assert_eq!(
            running.part("input").expect("the field").rect,
            field,
            "the box does not move when a tip's neighbours come and go"
        );
        assert_eq!(
            running.part("tip").expect("the row").rect.y + 1,
            field.y,
            "and the row is still the one against the field"
        );

        // `[[remove]] id = "tui-panel-tip"`: the composer closes up, and the row
        // goes back to the conversation rather than being left as a blank line.
        h.modules.remove_view("tip");
        let removed = h.compose((60, 12));
        assert!(removed.part("tip").is_none());
        assert_eq!(
            removed.part("input").expect("the field").rect,
            field,
            "the field does not move"
        );
        assert_eq!(
            removed.part("stream").expect("the conversation").rect.h,
            running.part("stream").expect("the conversation").rect.h + 1,
            "and the row is handed back to the words"
        );
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
    fn what_was_said_is_folded_from_the_log_not_kept_beside_it() {
        // Folded here rather than appended at submit, so a resumed session can
        // arrow back through what was said before the resume — the log is the
        // only thing that survives, and a second copy would drift from it.
        let h = fed();
        let history = h.moment.read().unwrap().history.clone();
        assert_eq!(history, vec!["fix the build", "now break it"]);

        // Consecutive repeats collapse, the way every shell collapses them.
        h.absorb(&SessionEvent::UserMessage {
            text: "now break it".into(),
            turn: 9,
            images: Vec::new(),
        });
        h.absorb(&SessionEvent::UserMessage {
            text: "   ".into(),
            turn: 10,
            images: Vec::new(),
        });
        assert_eq!(h.moment.read().unwrap().history, history, "{history:?}");
    }

    #[test]
    fn a_thought_and_a_tool_call_answer_a_click_but_prose_does_not() {
        // Two decisions, deliberately different. Prose is not a click target:
        // it is the largest surface on the screen, and folding away the answer
        // someone was reading is the one gesture that can lose work. A thought
        // or a tool call is a one-line lid, and a click on a lid has one meaning.
        //
        // A lid, specifically — not a hidden block. Reasoning opens off the
        // screen now, so this asks for its lid first: a click can only land on
        // something that has a row to land on.
        let h = fed();
        let size = (80, 40);
        h.presentation.write().unwrap().toggle("reasoning");
        let frame = h.compose(size);
        let rect = frame.part("stream").unwrap().rect;
        let kinds: Vec<&str> = (rect.y..rect.bottom())
            .filter_map(|y| h.block_at(2, y))
            .map(|(_, kind)| kind)
            .collect();
        assert!(!kinds.is_empty(), "nothing is clickable at all");
        assert!(
            kinds.iter().all(|k| *k == "tool_call" || *k == "reasoning"),
            "these answer a click too: {kinds:?}"
        );
        assert!(
            kinds.contains(&"reasoning"),
            "a thought is a lid: {kinds:?}"
        );
        assert!(
            kinds.contains(&"tool_call"),
            "a tool call is a lid: {kinds:?}"
        );
    }

    #[test]
    fn clicking_a_thought_opens_that_thought() {
        // The gesture that used to be missing: ctrl-r moves every thought in the
        // transcript at once, and the lid — the thing being pointed at —
        // answered nothing. Clicking the lid opens the lid.
        //
        // Reasoning opens off the screen, so the lid is asked for first: one
        // press of ctrl-r, which is the state that has a row to point at.
        let h = fed();
        let size = (80, 40);
        h.presentation.write().unwrap().toggle("reasoning");
        let folded = h.compose(size).rows().join("\n");
        assert!(folded.contains("思考"), "folded to a summary:\n{folded}");
        assert!(!folded.contains("hmm"), "the words are hidden, not gone");

        let rect = h.compose(size).part("stream").unwrap().rect;
        let (id, kind) = (rect.y..rect.bottom())
            .filter_map(|y| h.block_at(2, y))
            .find(|(_, kind)| *kind == "reasoning")
            .expect("the folded thought answers a click");
        h.presentation.write().unwrap().toggle_block(id, kind);
        let open = h.compose(size).rows().join("\n");
        assert!(
            open.contains("hmm"),
            "the click showed the working:\n{open}"
        );

        h.presentation.write().unwrap().toggle_block(id, kind);
        assert_eq!(
            h.compose(size).rows().join("\n"),
            folded,
            "clicking it again is the inverse"
        );
    }

    #[test]
    fn a_block_that_cannot_be_clicked_still_folds_by_key() {
        // 「能折叠」and「能点击」are two questions, and writing them as one
        // predicate once broke ctrl-r: making a kind unclickable silently made
        // it unfoldable too. Prose is the standing example — never a click
        // target, still foldable by key.
        assert!(
            !CLICKABLE.contains(&"assistant"),
            "prose is not a click target"
        );
        let h = fed();
        // A one-line answer would prove nothing: folded and open would render
        // the same row. The block has to be long enough that folding it is a
        // difference anyone could see.
        let answer = (1..=8)
            .map(|n| format!("第 {n} 行:一段足够长的回答,折叠起来和不折叠不是同一屏"))
            .collect::<Vec<_>>()
            .join("\n");
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 12,
            round: 0,
            text: answer,
            reasoning: String::new(),
            tool_calls: Vec::new(),
        });
        let open = h.compose((80, 40)).rows().join("\n");
        assert!(open.contains("第 8 行"), "the whole answer is up:\n{open}");
        h.presentation.write().unwrap().toggle("assistant");
        let folded = h.compose((80, 40)).rows().join("\n");
        assert_ne!(open, folded, "prose stopped folding by key");
        assert!(
            !folded.contains("第 8 行"),
            "folded means hidden:\n{folded}"
        );
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
            last.plain().contains("已中断"),
            "bottom-anchored, newest last: {:?}",
            last.plain()
        );
    }

    #[test]
    fn reasoning_is_off_the_screen_by_default_and_the_key_brings_it_back() {
        // The reasoning channel is the working, not the answer. A lid between
        // every call — `◐ 思考 7 行` — is a row spent telling someone who is not
        // reading the working how much working there is that they are not
        // reading. So it opens off the screen, and ctrl-r (or `/reasoning`)
        // brings it back: a lid, then the whole thought, then away again.
        let h = fed();
        let away = h.compose((80, 40)).rows().join("\n");
        assert!(!away.contains("思考"), "a lid is on screen:\n{away}");
        assert!(!away.contains("hmm"), "and the words with it");

        let hashes_before: Vec<_> = h
            .stream
            .read()
            .unwrap()
            .settled_hashes()
            .into_iter()
            .collect();
        h.presentation.write().unwrap().toggle("reasoning");
        let lid = h.compose((80, 40)).rows().join("\n");
        assert!(
            lid.contains("思考"),
            "the key did not bring it back:\n{lid}"
        );
        assert!(!lid.contains("hmm"), "and did not spend the screen yet");

        h.presentation.write().unwrap().toggle("reasoning");
        let open = h.compose((80, 40)).rows().join("\n");
        assert!(open.contains("hmm"), "expanding shows them");
        assert_eq!(
            h.stream.read().unwrap().settled_hashes(),
            hashes_before,
            "presentation changed; content did not"
        );

        // And round again. A key that can bring this back is only half a key if
        // it cannot put it away: reasoning is the one kind a reader may take off
        // the screen, and this is the only way back to that.
        h.presentation.write().unwrap().toggle("reasoning");
        assert_eq!(
            h.compose((80, 40)).rows().join("\n"),
            away,
            "the cycle does not come back round"
        );
    }

    #[test]
    fn a_hidden_kind_is_a_row_the_scroll_does_not_count() {
        // The blanks test with the sign flipped, and the same failure behind it:
        // the painter skipping a block is only half of that decision. If the
        // height goes on counting rows of a block nobody drew, the top of the
        // transcript sits one row further away than the screen has rows, and
        // scrolling to the limit walks the first thing said off the top.
        let h = fed();
        let size = (80, 12);
        let at_limit = |h: &Host| -> Vec<String> {
            let m = Moment::default();
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(h.scroll_limit(size, &m));
            h.compose(size)
                .part("stream")
                .expect("the conversation")
                .lines
                .iter()
                .map(|l| l.plain())
                .collect()
        };

        let away = at_limit(&h);
        assert!(
            away.iter().any(|r| r.contains("fix the build")),
            "hidden: scrolled to the limit and the first thing said is gone: {away:?}"
        );

        // And the same with it back on screen. The count has to be the painter's
        // in either state — one of the two is otherwise measured against a
        // screen that is not the one in front of anybody.
        h.presentation.write().unwrap().toggle("reasoning");
        let lid = at_limit(&h);
        assert!(
            lid.iter().any(|r| r.contains("fix the build")),
            "shown: scrolled to the limit and the first thing said is gone: {lid:?}"
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
    use crate::modules::{input, live, status, transcript};

    #[test]
    fn print_a_frame() {
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<live::Live>::new()))
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
        // A turn in flight, so the live line is in the picture: three seconds
        // and one tool call into the next thing the model was asked to do.
        h.moment.write().unwrap().now = crate::moment::Timestamp::millis(12_000);
        h.absorb(&atomcode_harness::session::SessionEvent::TurnStart { turn: 3 });
        h.absorb(&atomcode_harness::session::SessionEvent::AssistantMessage {
            turn: 3,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: "c4".into(),
                name: "bash".into(),
                arguments: r#"{"command":"cargo test -p atomcode-tui"}"#.into(),
            }],
        });
        // A real reading off the wire, so the figures in the frame are the shape
        // they take on a long turn: a context of tens of thousands of tokens
        // almost all served from cache, and the output of one round.
        h.absorb(&atomcode_harness::session::SessionEvent::Usage {
            turn: 3,
            round: 1,
            usage: atomcode_kernel::stream::TokenUsage {
                prompt: 57_252,
                completion: 1_107,
                cached: 56_192,
            },
        });
        {
            let mut m = h.moment.write().unwrap();
            m.input = "接下来呢".into();
            m.activity = crate::moment::Activity::Working;
            m.now = crate::moment::Timestamp::millis(15_000);
        }
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
