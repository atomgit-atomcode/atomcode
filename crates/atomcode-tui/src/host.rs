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
use crate::caps::{Caps, Glyph};
use crate::frame::{Frame, Line, Rect, Span, Style};
use crate::module::{Height, Modules};
use crate::moment::{Moment, Notice};
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

/// Whether a kind may be taken off the screen entirely.
///
/// Reasoning, and the environment's own injections — see
/// [`ENVIRONMENTAL_INJECTIONS`]. It is the working rather than the answer: the
/// two things on the stream that a person may reasonably want gone once they
/// have read them, and both open hidden for that reason. Everything else is
/// content: what was asked, what was answered, what a tool returned, what a
/// teammate reported. Putting a lid over those is a different decision, and
/// `ctrl-t` already makes it.
///
/// A predicate rather than a flag on the block, for the same reason [`CLICKABLE`]
/// is a list: it is about what the screen does, not about what the block is.
///
/// [`ENVIRONMENTAL_INJECTIONS`]: crate::content::ENVIRONMENTAL_INJECTIONS
fn hideable(kind: &str) -> bool {
    kind == "reasoning" || crate::content::ENVIRONMENTAL_INJECTIONS.contains(&kind)
}

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
    /// How the screen opens: reasoning and the environment's own injections
    /// away, tool calls behind a lid.
    ///
    /// Both are the working rather than the answer, and a transcript is read for
    /// the answer. Tool calls keep a lid where reasoning does not, because
    /// *what was run* is part of that answer: someone glancing at a transcript
    /// wants to know that a file was read, not what the model was thinking
    /// while it read it.
    ///
    /// Reasoning and the environment's own injections both open off the screen,
    /// and both come back one step at a time: a one-row lid, then the whole
    /// thing, then away again — `ctrl-r` for the first, `/showinject` for the
    /// second. The difference is the audience: a thought is the model working and
    /// a person may want to watch it arrive, while an injection is the harness
    /// talking to the model and nobody is reading `<system-reminder>` on purpose.
    /// Hiding it is the same decision either way.
    pub fn default_folds() -> Self {
        let mut by_kind: Vec<(&'static str, Showing)> = vec![
            ("reasoning", Showing::Hidden),
            ("tool_call", Showing::Folded),
        ];
        by_kind.extend(
            crate::content::ENVIRONMENTAL_INJECTIONS
                .iter()
                .map(|kind| (*kind, Showing::Hidden)),
        );
        Self {
            by_kind,
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
            Showing::Open if hideable(kind) => Showing::Hidden,
            Showing::Open => Showing::Folded,
        };
        self.set(kind, next);
        self.by_block.clear();
    }

    /// The same gesture over a group, each member stepped once.
    ///
    /// Each one steps on its own state rather than being set to the first
    /// member's: a group whose members have been driven apart by hand would
    /// otherwise snap them all to wherever the first one happened to be, which is
    /// the group gesture overruling the individual one rather than adding to it.
    pub fn toggle_many(&mut self, kinds: impl IntoIterator<Item = &'static str>) {
        for kind in kinds {
            self.toggle(kind);
        }
    }

    /// Say what a block is folded to, flatly.
    ///
    /// Flatly and not as a toggle, because a run of calls is set together: the
    /// host asks what the run is and writes the same answer to every member, and
    /// a per-block toggle would flip each one against its own state. The host is
    /// the only thing that folds one block, because a click has to know what a
    /// block's *run* is before it can mean anything — see `Host::toggle_block`.
    pub fn set_block(&mut self, id: BlockId, folded: bool) {
        self.by_block.insert(id, folded);
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

/// Which foldable block, and what kind, owns a painted row.
///
/// `None` for a row that belongs to nobody — a blank, or a question not yet in
/// the stream — and `Some` for one a click can fold. Named because the concrete
/// type appears twice in one signature and once more as the accumulator beside
/// it, and `Vec<Option<(BlockId, &'static str)>>` three times over is a type a
/// reader has to re-derive each time instead of recognising.
type RowOwner = Option<(BlockId, &'static str)>;

/// Whether a blank row goes on the seam between two stacked blocks.
///
/// `upper` is the block nearer the top of the screen, `lower` the one under it.
/// The order is part of the signature because one of the rules is not symmetric:
/// both call sites pass the pair this way round even though the painter walks
/// the stream backwards.
///
/// A tool call is its own paragraph. Without a row between them the model's
/// prose runs straight into the `●` header and the two read as one wall — the
/// sentence and the thing that was run at the same level. One blank row is the
/// whole of the fix, and it goes on *both* sides of a call, because
/// `⎿ ok · 12 行` followed by the next sentence has the same problem the other
/// way round — hence the two-sided test below.
///
/// Nothing goes between two tool calls. A run of them is one thought, and a
/// screen of six calls separated by five gaps is a screen that no longer shows
/// what was done in one glance.
///
/// The turn's closing rule is a separator — `──── ✓ 完成 · 3 步 ────` — and gets
/// a row of air on both sides unconditionally. It is the one row that is *about*
/// the transcript rather than part of it, and pressed against the prose above
/// and the next question below it stops reading as a boundary and starts
/// reading as one more line of the answer.
///
/// The user's message opens a paragraph downwards: the answer starts under a
/// blank rather than directly under the bar, where it would read as the first
/// line of what was asked rather than as a reply to it. Nothing is needed above
/// it, because what is normally there is the closing rule — which now carries
/// its own margin.
///
/// One definition, consulted by both the painter and the height the scroll is
/// measured against — the two have to agree or the last rows of a long
/// transcript become unreachable.
fn blank_between(upper: &str, lower: &str) -> bool {
    if upper == "turn_end" || lower == "turn_end" {
        return true;
    }
    if upper == "user" {
        return true;
    }
    (upper == "tool_call") != (lower == "tool_call")
}

/// What a block of this kind opens with, if it opens with anything.
///
/// A turn arrives as an interleaving of prose, tool calls and thoughts, and they
/// all used to begin in the same column: what the model *said* and what it *ran*
/// were told apart only by reading them. So the answer opens with the same `●` a
/// call does, and is set in by the width of it — the reply's words land in the
/// very column a call's `⎿` hangs in, which is what makes them one piece of
/// work rather than two that happen to sit near each other.
///
/// The mark is the colour of the words it opens rather than a colour of its own,
/// which is why it is uncoloured: body text states no colour by design (see
/// `theme`), and a mark that named one would come apart from its text the first
/// time the palette moved. It is `●` and not a new glyph because it is the same
/// mark — the screen says "here is a piece of this turn" in one shape.
///
/// The rule lives here, next to [`blank_between`], because a mark and a margin
/// are facts about how the screen shows a block and not about what the block
/// says: the same block at the same width is the same content wherever it is
/// drawn, and `content_hash` must not move because somebody changed the layout.
///
/// Users are out: their message is a full-width bar on purpose (see
/// `content::UserSaid`), and setting it in would eat the bar's whole point. A
/// thought is out too — it is chrome over the answer, and `· 思考 3 行` already
/// says what it is.
fn opener(kind: &str) -> Option<Span> {
    match kind {
        "assistant" => Some(Span::styled(
            format!("{} ", Caps::default().g(Glyph::ToolMark)),
            Style::new(),
        )),
        _ => None,
    }
}

/// How far a block of this kind is set in from the left edge, in cells.
///
/// The width of the [`opener`] and nothing else, so that "the words start where
/// the mark ends" is one fact rather than two that agree today.
fn inset(kind: &str) -> u16 {
    opener(kind).map_or(0, |mark| mark.width() as u16)
}

/// One row in from the left edge by the opener's width, never past `w`.
///
/// The opener goes on the first row that has anything on it rather than on row
/// zero: an answer may begin with a blank row of its own, and a `●` alone above
/// the answer is a mark pointing at nothing. The blank rows ahead of it still
/// take their width, invisibly, so the block stays a rectangle — which is why
/// the caller says which lead this row gets rather than leaving it to be worked
/// out here, where the rows above it are no longer in hand.
///
/// The cells carry no style, so they inherit the row's background rather than
/// naming one — the same reason [`blank_between`]'s row is [`Line::empty`].
fn set_in_row(line: &Line, lead: Span, w: u16) -> Line {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(lead);
    spans.extend(line.spans.iter().cloned());
    // The caller rendered into `w - pad`, so this cut should never bite. It is
    // here anyway because the other half of the same promise is asserted by
    // `content_never_draws_wider_than_it_was_given`: content never exceeds the
    // width it was given, whatever the reason. Asked before cutting, because
    // `Line::truncate` copies unconditionally and this is a row of a frame.
    let mut out = Line::from_spans(spans);
    if out.width() > w as usize {
        out = out.truncate(w as usize);
    }
    out
}

/// Whether a row is one a person would call empty.
fn blank_row(line: &Line) -> bool {
    line.spans.iter().all(|s| s.text.trim().is_empty())
}

// How many rows the frames composed on this thread have copied into themselves.
//
// Test-only, and per-thread so a frame composed by one test is not counted
// against another. Whether a frame costs the screen or the block cannot be seen
// in what it draws — the same rows come out either way — only in the work it
// did, which is the same reason `block::LIVE_RESUMES` exists.
#[cfg(test)]
thread_local! {
    static COPIED_ROWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Put the rows of a block that can land on the screen into the frame.
///
/// `lines` is the whole block, `rows` the window the scroll has left at — a
/// range into `lines`, already clamped to it by the caller. Only the window is
/// set in: a block may be taller than the whole rect, and the rows above it are
/// neither copied nor dropped, which is what keeps a frame's cost the screen's
/// rather than the block's.
///
/// Rows go in bottom-first: the buffer is filled from the foot of the screen up,
/// and is reversed once at the end.
fn window_into(
    out: &mut Vec<Line>,
    owner: &mut Vec<RowOwner>,
    lines: &[Line],
    rows: std::ops::Range<usize>,
    open: Option<Span>,
    w: u16,
    own: RowOwner,
) {
    let window = &lines[rows.clone()];
    #[cfg(test)]
    COPIED_ROWS.with(|n| n.set(n.get() + window.len() as u64));
    let Some(open) = open else {
        for line in window.iter().rev() {
            out.push(line.clone());
            owner.push(own);
        }
        return;
    };
    let blank = Span::styled(" ".repeat(open.width()), Style::new());
    // Which row of the window is the block's first with anything on it. Every
    // row before that one takes the margin and no mark, and a window that opens
    // below it takes the margin for all of its rows — the same rows in the same
    // order [`set_in_row`] would have been handed, block or no block.
    let marked = lines
        .iter()
        .position(|line| !blank_row(line))
        .filter(|at| *at >= rows.start)
        .map(|at| at - rows.start);
    for (i, line) in window.iter().enumerate().rev() {
        out.push(set_in_row(
            line,
            if marked == Some(i) {
                open.clone()
            } else {
                blank.clone()
            },
            w,
        ));
        owner.push(own);
    }
}

/// Whether a call may be shown behind the same lid as its neighbours.
///
/// A skill is out: it is drawn open (`always_open`), so it is not behind a lid
/// of its own and cannot be behind somebody else's. A hidden block is out too,
/// but it is *transparent* rather than a divider — see [`run_from`].
fn behind_a_lid(block: &crate::block::Block) -> bool {
    block.kind() == "tool_call" && !block.content.always_open()
}

/// Whether this slot is a call the screen actually draws, and so a member of a
/// run.
fn drawn_call(slots: &[crate::block::Slot], i: usize, pres: &Presentation) -> bool {
    !pres.is_hidden(slots[i].block().kind()) && behind_a_lid(slots[i].block())
}

/// The run of drawn calls starting at `start`, and the slot the scan stopped on.
///
/// A hidden block is stepped over, not stopped at. A reader cannot see one — it
/// draws no rows — so it is not a seam in what was done: two calls with a
/// thought between them ran back to back, and the thought is not a reason to
/// spend a second `●` on them. Counting a hidden slot as the end of a run put
/// every call of a working session behind a lid of its own, because the model
/// thinks between calls — four calls that ran back to back, drawn as four rows
/// that each said `1`.
fn run_from(
    slots: &[crate::block::Slot],
    start: usize,
    pres: &Presentation,
) -> (Vec<usize>, usize) {
    let mut members = vec![start];
    let mut j = start + 1;
    while j < slots.len() {
        if pres.is_hidden(slots[j].block().kind()) {
            j += 1;
            continue;
        }
        if !behind_a_lid(slots[j].block()) {
            break;
        }
        members.push(j);
        j += 1;
    }
    (members, j)
}

/// The run a slot's rows are drawn as, when they are drawn behind one lid.
#[derive(Clone, Copy)]
struct Run {
    /// The slot the lid is drawn at: the run's last call. The painter walks the
    /// stream backwards and so reaches it first, and the rows of the calls
    /// before it are painted there.
    last: usize,
    /// How many calls are behind the lid. A count of *calls*, not the width of
    /// the span: a hidden block inside a run takes a slot and is not one of
    /// them, and `4 个工具` over three calls would be a lie about what ran.
    count: usize,
}

/// Which slots a lid answers for.
///
/// A run of calls is one piece of work — four calls that each said nothing are
/// four rows of noise, and the transcript is read for what was done, not for
/// how many round trips it took. So a run of consecutive folded calls is drawn
/// as one lid: how many there were, the last command, and its result.
///
/// Only *folded* runs merge. A call the reader has opened is the one thing they
/// are looking at, and burying it back inside a lid would answer a click by
/// taking the answer away. It also makes the merged form a pure consequence of
/// the fold state: nothing else has to be kept in step.
struct Lids {
    /// Per slot: the run it belongs to, when it is in one that merges. One
    /// vector rather than a map and a set, because this is asked once per slot
    /// per frame and the runs *are* contiguous.
    runs: Vec<Option<Run>>,
}

impl Lids {
    /// The run drawn at this slot, when this slot is the last of a merged run.
    ///
    /// The last member, because the painter walks the stream backwards and so
    /// reaches it first — the lid is drawn there and covers the rest.
    fn at(&self, i: usize) -> Option<Run> {
        self.runs.get(i).copied().flatten().filter(|r| r.last == i)
    }

    /// Whether this slot's rows were already painted by a lid.
    fn covers(&self, i: usize) -> bool {
        self.runs
            .get(i)
            .copied()
            .flatten()
            .is_some_and(|r| r.last != i)
    }
}

fn lids(slots: &[crate::block::Slot], pres: &Presentation) -> Lids {
    let mut runs: Vec<Option<Run>> = vec![None; slots.len()];
    let mut i = 0usize;
    while i < slots.len() {
        if !drawn_call(slots, i, pres) {
            i += 1;
            continue;
        }
        let (members, next) = run_from(slots, i, pres);
        let all_folded = members
            .iter()
            .all(|m| pres.is_block_folded(slots[*m].block().id, "tool_call"));
        if members.len() > 1 && all_folded {
            let run = Run {
                last: *members.last().expect("a run has a first member"),
                count: members.len(),
            };
            for m in members {
                runs[m] = Some(run);
            }
        }
        i = next;
    }
    Lids { runs }
}

/// The one lid a merged run is drawn as, from its last call.
fn lid_lines(
    slots: &[crate::block::Slot],
    last: usize,
    count: usize,
    w: u16,
) -> Vec<crate::frame::Line> {
    match slots[last].block().content.as_tool_call() {
        Some(call) => crate::content::ToolCallBlock::group_lines(call, count, w),
        // Unreachable while `behind_a_lid` and `as_tool_call` agree; a row of
        // nothing is what keeps a disagreement from taking the screen down.
        None => Vec::new(),
    }
}

/// The run of calls a block belongs to, itself included.
///
/// The same run `lids` would merge, asked from a block instead of from the
/// stream: one call alone for anything that is not a call, so a click on prose
/// or on a thought folds exactly what it landed on. Same membership rule — a
/// hidden block is stepped over — because a lid that says `4 个工具` and then
/// hands over three of them is a lie about what was behind it.
fn run_around(
    slots: &[crate::block::Slot],
    pres: &Presentation,
    id: crate::block::BlockId,
) -> Vec<crate::block::BlockId> {
    let Some(at) = slots.iter().position(|s| s.block().id == id) else {
        return vec![id];
    };
    if !behind_a_lid(slots[at].block()) {
        return vec![id];
    }
    // Back to the run's first call: over the members, and over the hidden slots
    // between them, stopping at the first thing that is neither.
    let mut start = at;
    let mut j = at;
    while j > 0 {
        j -= 1;
        if pres.is_hidden(slots[j].block().kind()) {
            continue;
        }
        if !behind_a_lid(slots[j].block()) {
            break;
        }
        start = j;
    }
    let (members, _) = run_from(slots, start, pres);
    members.into_iter().map(|m| slots[m].block().id).collect()
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

/// What a pointer press did to the open context menu.
///
/// Three outcomes rather than a bool, because "nothing was open" and "what was
/// open was not clicked" lead to different things: the first falls through to
/// the ordinary click handling, the second dismisses and *also* falls through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextClick {
    /// No menu was open.
    NotOpen,
    /// A menu was open and the press was not on it. The caller has already had
    /// it closed for them.
    Outside,
    /// The press chose an item, or dismissed the menu with nothing.
    Picked(crate::menu::Step),
}

/// A line widened to the rect with the panel's own style.
///
/// A floating part covers what it is drawn over only where it puts a cell down,
/// and a text span ends where its text ends. Filling the rest of the row is what
/// makes the menu a surface over the conversation rather than words with the
/// conversation visible through them.
fn pad(line: Line, w: usize, style: Style) -> Line {
    let used = line.width();
    if used >= w {
        return line.truncate(w);
    }
    let mut spans = line.spans;
    spans.push(Span::styled(" ".repeat(w - used), style));
    Line::from_spans(spans).truncate(w)
}

/// Everything the screen is composed from.
pub struct Host {
    pub stream: RwLock<Stream>,
    /// Slash commands, contributed by rows.
    pub commands: Arc<crate::command::Commands>,
    /// The slash menu — what to show while a command is being typed.
    ///
    /// Kept here rather than in the input module because it is two different
    /// concerns in one place: the host owns the command registry, and the menu
    /// is drawn *over* the layout rather than inside the field. A module that
    /// owned it would have to be told, and a module that draws it inside its
    /// own rect is a module that resizes the conversation when a slash is
    /// typed. Empty means nothing to suggest.
    menu: RwLock<Vec<(String, String)>>,
    /// The composer's context menu, when the secondary button opened one.
    ///
    /// Beside the slash menu and for the same reason: it is drawn *over* the
    /// layout rather than inside the field, so nothing is resized to make room
    /// for it. `None` means nothing is open.
    context_menu: RwLock<Option<crate::menu::Menu>>,
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
            menu: RwLock::new(Vec::new()),
            context_menu: RwLock::new(None),
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

    /// Replace the slash menu. Empty closes it.
    ///
    /// Set by whoever owns the command registry — the front end — because the
    /// menu's *contents* are a question about commands, not about the screen.
    /// Where it is drawn is this struct's business, not the caller's.
    pub fn set_menu(&self, menu: Vec<(String, String)>) {
        *self.menu.write().expect("menu poisoned") = menu;
    }

    /// Open the composer's context menu at a cell. Empty items opens nothing.
    ///
    /// The host keeps the open menu because the host is what draws it — a
    /// module cannot draw outside its own rect, and this panel is deliberately
    /// drawn outside the field's. What is *in* it, and what picking an item
    /// means, stays with the caller.
    pub fn open_context_menu(&self, at: (u16, u16), items: Vec<crate::menu::Item>) {
        *self.context_menu.write().expect("menu poisoned") = crate::menu::Menu::new(at, items);
    }

    /// Close it, if it is open. Returns whether there was one, so a caller can
    /// tell "closed it" from "there was nothing to close".
    pub fn close_context_menu(&self) -> bool {
        self.context_menu
            .write()
            .expect("menu poisoned")
            .take()
            .is_some()
    }

    pub fn context_menu_open(&self) -> bool {
        self.context_menu.read().expect("menu poisoned").is_some()
    }

    /// Say something on the reserved row above the field, for
    /// [`NOTICE_MS`](crate::moment::NOTICE_MS) and then no longer.
    ///
    /// The place for anything the screen has to report that is *about now*: a
    /// clipboard write, a refusal, a mode change. Deliberately not the stream —
    /// a block for "已复制" pushes the whole conversation up a row for a sentence
    /// nobody reads twice, and a block is for what happened, not for what has
    /// just become true. The row is already reserved for it, so saying this
    /// moves nothing.
    ///
    /// The expiry is stamped here, where the clock is, and travels with the text:
    /// the module that draws it compares two readings it was handed rather than
    /// asking a clock of its own (`docs/adr/0008`).
    ///
    /// `refused` is the same distinction `content::CommandSaid` draws — it could
    /// not be done — so "已复制" and "没有可复制的内容" never look alike.
    pub fn say(&self, text: impl Into<String>, refused: bool) {
        let mut m = self.moment.write().expect("moment poisoned");
        let now = m.now;
        m.notice = Some(Notice::for_ms(text, refused, now, crate::moment::NOTICE_MS));
    }

    /// Run a key against the open menu, returning the step it produced. `None`
    /// when nothing is open, so the caller falls through to the ordinary keys —
    /// focus is arbitration, and this is where the menu is given it or not.
    pub fn context_menu_key(&self, press: crate::surface::KeyPress) -> Option<crate::menu::Step> {
        let mut held = self.context_menu.write().expect("menu poisoned");
        let menu = held.as_mut()?;
        let step = menu.key(press);
        if !matches!(step, crate::menu::Step::Stay) {
            *held = None;
        }
        Some(step)
    }

    /// A pointer move over the open menu: point at the row it is over. `true`
    /// when that changed which row is pointed at, and so a frame is owed.
    ///
    /// A move that changes no row answers `false` for the reason the whole
    /// gesture is affordable: the terminal reports one of these per cell the
    /// pointer crosses, and painting a frame for each would trade a cheap
    /// highlight for a busy one.
    ///
    /// Nothing is open is not an error and not news: the pointer moves over the
    /// screen all the time.
    pub fn context_menu_hover(&self, x: u16, y: u16, size: (u16, u16)) -> bool {
        let mut held = self.context_menu.write().expect("menu poisoned");
        let Some(menu) = held.as_mut() else {
            return false;
        };
        menu.hover(x, y, size.0, size.1)
    }

    /// A pointer press against the open menu. `NotOpen` when nothing is open;
    /// `Outside` when it was open but the press was elsewhere, which the caller
    /// treats as "dismiss, and let the press mean whatever it meant".
    ///
    /// The screen size is a parameter rather than a field because the surface is
    /// the authority on it and this struct has no business keeping a second
    /// copy that could disagree by a frame.
    pub fn context_menu_click(&self, x: u16, y: u16, size: (u16, u16)) -> ContextClick {
        let mut held = self.context_menu.write().expect("menu poisoned");
        let Some(menu) = held.as_mut() else {
            return ContextClick::NotOpen;
        };
        match menu.click(x, y, size.0, size.1) {
            Some(step) => {
                *held = None;
                ContextClick::Picked(step)
            }
            // A press anywhere else puts it away. The menu is a thing that was
            // raised over the screen, and the first press that is not for it is
            // someone done with it — leaving it up would mean a click on the
            // conversation both did its own thing and left a panel behind.
            None => {
                *held = None;
                ContextClick::Outside
            }
        }
    }

    /// How tall the slash menu would like to be, and at most what it may be.
    ///
    /// Capped well short of the screen so it reads as something that rose out
    /// of the prompt rather than as a second transcript.
    fn menu_rows(&self, menu: &[(String, String)]) -> u16 {
        (menu.len() as u16).clamp(1, 10)
    }

    /// Where the menu rises to, over the layout.
    ///
    /// Bottom-anchored to the input field's own top edge and one row taller
    /// than the list, so its first row is a blank margin against the field's
    /// top rule: the menu appears to grow *out of* the prompt. Every row above
    /// that top edge is drawn over whatever the layout put there, and nothing
    /// is given up for it — the rects of the stream, the composer and the
    /// status bar are unchanged by the menu being open.
    fn menu_rect(&self, frame: &Frame, screen: Rect, rows: u16) -> Option<Rect> {
        let field = frame.part(crate::modules::input::ID)?.rect;
        // One for the margin, and no more than fits above the field.
        let want = rows.saturating_add(1).min(field.y);
        if want == 0 {
            return None;
        }
        let x = field.x;
        let w = field.w.min(screen.w.saturating_sub(x));
        if w == 0 {
            return None;
        }
        let y = field.y - want;
        // `want` rows bottom-aligned in the rect, so the last of them is the
        // margin and the list itself hangs from there.
        Some(Rect::new(x, y, w, want))
    }

    /// The menu's rows, top to bottom in `rect`, with its margin last.
    fn menu_lines(&self, rect: Rect, menu: &[(String, String)]) -> Vec<Line> {
        let w = rect.w as usize;
        let mut out: Vec<Line> = Vec::with_capacity(rect.h as usize);
        let room = (rect.h as usize).saturating_sub(1);
        let style = crate::theme::bg(crate::theme::Role::PanelBg)
            .under(crate::theme::fg(crate::theme::Role::PanelFg));
        for i in 0..room {
            let line = match menu.get(i) {
                Some((name, about)) => {
                    let mut spans = vec![
                        Span::styled("  /".to_string(), style),
                        Span::styled(
                            name.clone(),
                            crate::theme::fg(crate::theme::Role::Accent).under(style),
                        ),
                    ];
                    if !about.is_empty() {
                        spans.push(Span::styled(
                            format!("  {about}"),
                            crate::theme::fg(crate::theme::Role::Muted).under(style),
                        ));
                    }
                    Line::from_spans(spans)
                }
                // Never reached in practice: the rect is sized to the list, and
                // this is what keeps a short list from showing the screen
                // through its own panel.
                None => Line::empty(),
            };
            out.push(pad(line, w, style));
        }
        // The margin: one blank row of the panel's colour, so the list reads as
        // a surface lifted off the prompt rather than as text floating on it.
        out.push(Line::styled(" ".repeat(w), style).truncate(w));
        out
    }

    /// Render the stream's tail into `rect`.
    ///
    /// Bottom-anchored: what a person is reading is the newest thing. Blocks
    /// are rendered newest-first until the rect is full, then reversed — so the
    /// cost is O(what fits), not O(the conversation).
    fn stream_lines(&self, rect: Rect) -> (Vec<Line>, Vec<RowOwner>) {
        let stream = self.stream.read().expect("stream poisoned");
        let pres = self.presentation.read().expect("presentation poisoned");
        let mut out: Vec<Line> = Vec::new();
        // Grown in lockstep with `out`, so a row and its owner cannot get out
        // of step — the alternative is two loops that agree until one changes.
        let mut owner: Vec<RowOwner> = Vec::new();
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

        let lids = lids(stream.slots(), &pres);

        for (i, slot) in stream.slots().iter().enumerate().rev() {
            // The earlier calls of a merged run were drawn by the lid at the end
            // of it, which this backwards walk reached first.
            if lids.covers(i) {
                continue;
            }
            let block = slot.block();
            let kind = block.kind();
            // A kind the reader has taken off the screen draws nothing at all —
            // not even the blank row, because it is not a neighbour of anything
            // either. Asked before the `below` bookkeeping, so a hidden block
            // leaves the seam between its visible neighbours the way it was.
            if pres.is_hidden(kind) {
                continue;
            }
            let lid = lids.at(i);
            // Two different questions. Reasoning and tool calls both fold — that
            // is what ctrl-r and ctrl-t are — but what a click may fold is
            // narrower than what folds: prose is out, because it is what the
            // transcript is for and most of the screen is things the model said.
            let foldable = !block.content.always_open();
            let mut own = (foldable && CLICKABLE.contains(&kind)).then_some((block.id, kind));
            // The column this block leaves on the left, and the width that is
            // actually left to draw in. Every render below is asked for `room`,
            // never `rect.w` — content wrapped to the full width and then set in
            // would overflow, and the cut in `set_in_row` would eat its last
            // cells.
            let pad = inset(kind);
            let room = rect.w.saturating_sub(pad);
            let lines: Arc<Vec<Line>> = if let Some(run) = lid {
                // A run of folded calls behind one lid. Its rows are owned by
                // the last call, so a click anywhere on the lid folds the run
                // that drew it — which is the only thing that click could mean.
                own = Some((block.id, kind));
                Arc::new(lid_lines(stream.slots(), i, run.count, room))
            } else if foldable && pres.is_block_folded(block.id, kind) {
                Arc::new(vec![block.content.summary(room)])
            } else {
                // The one place a block is rendered for the screen. Its row count
                // comes first, because a settled block already knows it: a block
                // that lies entirely above the reader is then skipped in
                // arithmetic instead of being rendered and thrown away, which is
                // what makes scrolling back through a long conversation cost the
                // screen rather than the session.
                let (n, rendered) = slot.rows_at(room);
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
                    if below.is_some_and(|b| blank_between(kind, b)) {
                        skipped += 1;
                    }
                    below = Some(kind);
                    continue;
                }
                // A hit hands back the rows it measured with — the block's own,
                // shared rather than copied, since a frame wants a screenful of
                // them at most. A miss is rendered now, for the same reason the
                // count came first: this is a block the reader can see.
                match rendered {
                    Some(lines) => lines,
                    None => Arc::new(block.content.lines(room)),
                }
            };
            if lines.is_empty() {
                continue;
            }
            // A lid's rows are a run's, not one block's, so `rows_at` cannot
            // measure them and the same skip is decided here instead — same
            // arithmetic, same reason.
            let n = lines.len();
            if out.len() >= want {
                break;
            }
            if n <= scroll.saturating_sub(skipped) {
                skipped += n;
                if below.is_some_and(|b| blank_between(kind, b)) {
                    skipped += 1;
                }
                below = Some(kind);
                continue;
            }
            let blank = below.is_some_and(|b| blank_between(kind, b));
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
            // The margin is added as the rows go in, and only to the rows that go
            // in: the two renderers a block has — `lines` and the incremental
            // `render_settled` the live cache drives — are both reached above, so
            // one call covers a streaming answer and a settled one alike. Doing
            // it inside the block would mean doing it twice, and the two would
            // drift the moment one of them was missed.
            //
            // Rows counted from the bottom, because the scroll offset is: the
            // first `scroll - skipped` of them are above the window and are
            // stepped over, and of what is left the rect takes what it has room
            // for. Everything above that is not copied at all — a block can be
            // taller than the screen, and it is the screen the frame costs.
            let from_bottom = scroll.saturating_sub(skipped).min(n);
            let take = want.saturating_sub(out.len()).min(n - from_bottom);
            skipped += from_bottom;
            window_into(
                &mut out,
                &mut owner,
                &lines,
                n - from_bottom - take..n - from_bottom,
                opener(kind),
                rect.w,
                own,
            );
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

        // The slash menu rises over the layout, out of the prompt. Drawn here,
        // after every region has its rect, so it composes as an overlay rather
        // than as a region: nothing above the field is resized to make room,
        // and the rows it covers are covered rather than taken away.
        let menu = self.menu.read().expect("menu poisoned").clone();
        if !menu.is_empty() {
            if let Some(rect) = self.menu_rect(&frame, Rect::sized(w, h), self.menu_rows(&menu)) {
                frame.place("menu", rect, self.menu_lines(rect, &menu));
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

        // Last of all, and after the selection on purpose: the context menu is
        // a panel that was raised over the screen, so it wins every cell it
        // covers. Drawn before the highlight it would be recoloured by a
        // selection that runs under it — the menu's own rows came out striped
        // where a selection crossed them, which is what a menu covering what it
        // covers is not supposed to look like.
        if let Some(menu) = self.context_menu.read().expect("menu poisoned").clone() {
            let rect = menu.rect(w, h);
            if !rect.is_empty() {
                let vp = crate::moment::Viewport::new(rect, &moment);
                frame.place("context-menu", rect, menu.render(&vp));
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
        // Walking forwards, so the block seen last is the one *above* the
        // current one — `blank_between` takes (upper, lower).
        let mut above: Option<&'static str> = None;
        let lids = lids(stream.slots(), &pres);
        for (i, slot) in stream.slots().iter().enumerate() {
            // A merged run is one row-block: its earlier members were drawn by
            // the lid at the end of it, so they are not rows and not neighbours.
            if lids.covers(i) {
                continue;
            }
            let b = slot.block();
            // What is not drawn is not a row of the transcript — and by the same
            // token it is not a neighbour, so the blanks around it stay put.
            if pres.is_hidden(b.kind()) {
                continue;
            }
            // The same width the painter will use, and for the same reason it
            // exists: a row count is only the painter's if it was measured at
            // the width the painter draws at, and a block that is set in draws
            // two cells narrower. Measuring here at the full width would count
            // the wraps of a *wider* column than the one on screen, so a long
            // reply would come out shorter in the sum than it is in the frame —
            // and the last rows of it would be exactly that many rows out of
            // reach at the bottom of the scroll.
            let room = width.saturating_sub(inset(b.kind()));
            let n = match lids.at(i) {
                Some(run) => lid_lines(stream.slots(), i, run.count, room).len(),
                None if !b.content.always_open() && pres.is_block_folded(b.id, b.kind()) => 1,
                None => slot.rows_at(room).0,
            };
            if n == 0 {
                continue;
            }
            if above.is_some_and(|k| blank_between(k, b.kind())) {
                total += 1;
            }
            above = Some(b.kind());
            total += n;
        }
        total
    }

    /// The block under a point on the last painted frame, if it is one that
    /// can be folded.
    pub fn block_at(&self, x: u16, y: u16) -> Option<(BlockId, &'static str)> {
        self.hits.lock().expect("hits poisoned").at(x, y)
    }

    /// Fold or unfold what a click landed on.
    ///
    /// A run of calls is drawn as one lid, so a click on it opens the whole run:
    /// a lid that says `4 个工具` and then hands over one of them would be a lie
    /// about what was behind it. The run is found by the block clicked, not by
    /// where the lid is drawn, so the same call answers the same way whether it
    /// is behind a lid or open on its own.
    ///
    /// One state for the whole run, both ways: folding any call of an open run
    /// puts the run away. Keeping them in step is what makes the merged form a
    /// consequence of the fold state rather than a second thing to maintain.
    pub fn toggle_block(&self, id: BlockId, kind: &str) {
        let stream = self.stream.read().expect("stream poisoned");
        let slots = stream.slots();
        // Read first and let the guard go: the run is asked of the state a click
        // was answered against, and the write below needs the lock to itself.
        let run = {
            let pres = self.presentation.read().expect("presentation poisoned");
            run_around(slots, &pres, id)
        };
        let mut pres = self.presentation.write().expect("presentation poisoned");
        // `is_block_folded` and not the raw choice: a call nobody has spoken
        // about follows its kind, and every kind but reasoning defaults to
        // folded.
        let folded = pres.is_block_folded(id, kind);
        for block in run {
            pres.set_block(block, !folded);
        }
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
            .filter(|s| matches!(s, Slot::Live(..)))
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

/// The composer: the task list, the live line, and the reserved tip row above
/// the field.
///
/// One definition, because every arrangement that has an input has this above
/// it — the input box is where a person's eyes are, and the one thing that
/// answers "is it stuck?" belongs against it rather than in a panel beside the
/// conversation. The task list is here for the same reason: it is what the
/// current turn is working through, and it reads as part of the working rather
/// than as a panel about it.
///
/// Written into the tree rather than claimed by each row's own `LayoutOp::Show`
/// the way the mascot claims its strip: `Show` has two sides, above the
/// conversation and below the status line, and both are the wrong side of the
/// input box. That is why the todo list — which used to place itself at the
/// bottom, under the field — is a member here instead.
///
/// All three are `Hug`-ish: the live line and the task list ask for no rows
/// between turns and no panel until the model has planned something, so the
/// composer closes up around the field instead of standing on blank rows. The
/// tip row beneath is the deliberate exception — it asks for its row always,
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
            Item::hug(Region::view(crate::modules::todo::ID)),
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
        // `drop(s)` is load-bearing and `drop(w)` was not: `RwLockWriteGuard` has
        // a `Drop`, so its borrow of the stream lasts to the end of the scope and
        // `h` could not be moved without it; `StreamWriter` has none, so its
        // borrow already ended at its last use above.
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
    fn a_frame_copies_the_screen_not_the_block() {
        // One block taller than the rect, and a reader who can only see the
        // rect: only the rect is set in and copied. Asserted on the rows copied
        // rather than on the frame, because the same rows come out of a frame
        // that copied four hundred others on the way past.
        //
        // This is about the *block*, not the session — what is above the fold is
        // already skipped in arithmetic. A block bigger than the screen is the
        // same promise one level down: the rows it draws are the rect's, and the
        // rows it does not draw are nobody's cost.
        let h = host();
        {
            let mut s = h.stream.write().unwrap();
            s.writer("bench").emit(
                crate::block::Coord::default(),
                Arc::new(Counted {
                    lines: (0..400).map(|i| format!("row {i}")).collect(),
                    asked: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
            );
        }
        let size = (80, 12);
        // Two frames: the first has to measure the block, the second is the
        // steady state a scroll or a tick pays for.
        let _ = h.compose(size);
        COPIED_ROWS.with(|rows| rows.set(0));
        let frame = h.compose(size);
        let drawn = frame.part("stream").expect("the conversation").lines.len();
        assert!(
            drawn > 0 && drawn < 400,
            "the block is taller than the rect: {drawn} rows were drawn"
        );
        let copied = COPIED_ROWS.with(|rows| rows.get());
        assert_eq!(
            copied, drawn as u64,
            "the frame copied {copied} rows into a {drawn}-row rect: rows it \
             could not draw were set in on the way, so a frame cost the block \
             rather than the screen"
        );
    }

    #[test]
    fn a_frame_has_a_status_bar_a_conversation_and_a_prompt() {
        let h = fed();
        // Tall enough for the whole conformance corpus: the point here is that
        // the three regions coexist and the stream holds real content, and at a
        // height the conversation does not fit, the oldest row — the user's own
        // question — is correctly the first thing to go.
        let f = h.compose((80, 40));
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
        // The other half of the same decision: a run of tools is one thought, so
        // nothing goes between them — not a blank row, and now not a row of their
        // own each either. The run is drawn as one lid whose rows are adjacent.
        let h = fed();
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        let count = rows
            .iter()
            .position(|r| r.contains("2 个工具"))
            .expect("the run's lid");
        for row in &rows[count..count + 3] {
            assert!(
                !row.trim().is_empty(),
                "a blank row was put inside the run: {:?}",
                &rows[count..count + 3]
            );
        }
        assert!(
            rows[count + 1].contains("read_file(b.rs)"),
            "the last call is not the row under the count: {:?}",
            &rows[count..count + 3]
        );
    }

    #[test]
    fn a_run_of_folded_calls_is_one_lid_that_says_how_many() {
        // 「合并工具块」. A run of calls is one piece of work, and four rows that
        // each said nothing are four rows of noise. The lid says how many there
        // were, and shows the *last* command and its result — the run ends with
        // the thing that was being looked for.
        let h = fed();
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        let count = rows
            .iter()
            .position(|r| r.contains("2 个工具"))
            .expect("the lid does not say how many calls there were");
        assert!(
            !rows.iter().any(|r| r.contains("read_file(a.rs)")),
            "the first call is still on the screen, so nothing merged:\n{rows:#?}"
        );
        // The last call, on the rows under the count: the command, then what it
        // returned — the same two rows a single folded call draws.
        assert!(
            rows[count + 1].contains("read_file(b.rs)"),
            "the last command is not under the count: {:?}",
            &rows[count..count + 3]
        );
        assert!(
            rows[count + 2].contains("失败"),
            "the last call's result is not under it: {:?}",
            &rows[count..count + 3]
        );
    }

    #[test]
    fn clicking_the_lid_opens_every_call_in_the_run() {
        // 「点击要全部展开…有多个也要展开」. One row stands for several calls, so
        // the click has to hand back all of them — commands and outputs. Opening
        // one of them would be a lid that answered a click by keeping the rest
        // of what it was covering.
        let h = fed();
        let size = (80, 40);
        let before = h.compose(size).rows().join("\n");
        assert!(before.contains("2 个工具"), "nothing merged:\n{before}");

        let rect = h.compose(size).part("stream").unwrap().rect;
        // The count row is the lid's, and a click on it lands on the last call.
        let (id, kind) = (rect.y..rect.bottom())
            .filter_map(|y| h.block_at(2, y))
            .next()
            .expect("the lid answers a click");
        h.toggle_block(id, kind);

        let open = h
            .compose(size)
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>();
        let text = open.join("\n");
        assert!(
            !text.contains("2 个工具"),
            "the lid is still drawn after the click:\n{text}"
        );
        // Every call of the run is open: its command on one row and its result on
        // the next. A call left folded would have put the result on the command's
        // own row — which is exactly the difference this click has to make, and
        // the reason a weaker assertion here would pass while one of the two
        // calls was still behind a lid.
        for (name, result) in [
            ("read_file(a.rs)", "fn main() {}"),
            ("read_file(b.rs)", "no such file"),
        ] {
            let head = open
                .iter()
                .position(|r| r.contains(name))
                .unwrap_or_else(|| panic!("{name} never appeared:\n{text}"));
            assert!(
                !open[head].contains(result),
                "{name} is still a one-line lid: {:?}",
                &open[head..head + 2]
            );
            assert!(
                open[head + 1].contains(result),
                "{name} did not open onto its result: {:?}",
                &open[head..head + 2]
            );
        }
    }

    #[test]
    fn only_consecutive_calls_share_a_lid() {
        // The corpus has a second call after a notice and an injection. A lid
        // that swallowed it would report `3 个工具` over three things that did not
        // happen together.
        let h = fed();
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            !rows.iter().any(|r| r.contains("3 个工具")),
            "a lid swallowed a call from another run:\n{rows:#?}"
        );
        // The lone call is drawn as itself, with no count over it.
        assert!(
            rows.iter().any(|r| r.contains("看看那个目录")),
            "the second turn's call is missing:\n{rows:#?}"
        );
    }

    /// A thought between two calls does not end the run.
    ///
    /// The reader cannot see the thought: reasoning is hidden, so it draws no
    /// rows and is not a seam in what was done. Treating it as one — which is
    /// what the scan did, whatever its comment said — put *every* call of a
    /// working session behind a lid of its own: the model thinks before each
    /// call, so four calls that ran back to back were drawn as four rows that
    /// each said `1`.
    #[test]
    fn a_hidden_thought_between_two_calls_does_not_break_the_run() {
        let h = host();
        let call = |round: u32, id: &str| SessionEvent::AssistantMessage {
            turn: 1,
            round,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: id.into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            }],
        };
        h.absorb(&call(1, "c1"));
        h.absorb(&SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: "c1".into(),
            content: "a.rs".into(),
            is_error: false,
            images: Vec::new(),
        });
        // The working between the two calls, as it really arrives: a reasoning
        // chunk, then the message that made the call.
        h.absorb(&SessionEvent::AssistantChunk {
            turn: 1,
            round: 2,
            delta: "hmm".into(),
            reasoning: true,
        });
        h.absorb(&call(2, "c2"));
        h.absorb(&SessionEvent::ToolResultLogged {
            turn: 1,
            round: 2,
            call_id: "c2".into(),
            content: "a.rs".into(),
            is_error: false,
            images: Vec::new(),
        });

        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        // The premise, or the test guards nothing: the thought is on no row.
        assert!(
            !rows.iter().any(|r| r.contains("思考")),
            "reasoning is on screen, so this is not the case under test:\n{rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("2 个工具")),
            "the run was cut in two by a block nobody can see:\n{rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("1 个工具")),
            "a lid over one call:\n{rows:#?}"
        );
    }

    #[test]
    fn a_single_call_is_not_a_run_of_one() {
        // A lid over one call would say `1 个工具` and then show the call — a row
        // spent saying nothing. The count only exists where there is something
        // to count.
        let h = fed();
        let rows = h.compose((80, 40)).rows().join("\n");
        assert!(
            !rows.contains("1 个工具"),
            "a count over a single call:\n{rows}"
        );
    }

    #[test]
    fn the_users_message_opens_a_paragraph_the_answer_starts_under() {
        // What you asked is the row you scan for. With the reply's first row
        // directly under the bar, the bar reads as the opening line of the
        // answer — the same collapse as prose running into a `●` header.
        //
        // `now break it` is the case that pins both halves at once: the turn
        // above it ends in a closing rule (whose own margin is the row above
        // the bar), and the model's prose begins under the bar.
        let h = fed();
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        let asked = rows
            .iter()
            .position(|r| r.contains("now break it"))
            .expect("what was asked");
        assert!(
            rows[asked + 1].trim().is_empty(),
            "the answer starts on the bar instead of under it: {:?}",
            &rows[asked..asked + 3]
        );
        assert!(
            rows[asked + 2].contains("看看那个目录"),
            "and the row after the blank is the model's: {:?}",
            &rows[asked..asked + 3]
        );
    }

    /// How far `row` starts from the left edge, in cells.
    fn leading(row: &str) -> usize {
        row.len() - row.trim_start().len()
    }

    /// The column a row's *words* start in, past the mark that opens it.
    ///
    /// An answer opens with the same `●` a call does, so "the answer is set in"
    /// and "the answer lines up with its result" are two different numbers on
    /// one row. This is the second: `● Looking.` has its words in column two,
    /// and so does `  ⎿ 20 行`, which is the pair that has to agree.
    fn words(row: &str) -> usize {
        let mark = format!("{} ", Caps::default().g(Glyph::ToolMark));
        if row.starts_with(&mark) {
            crate::width::str_width(&mark)
        } else {
            leading(row)
        }
    }

    /// The rows the conversation drew, newest last.
    fn stream_rows(h: &Host, size: (u16, u16)) -> Vec<String> {
        h.compose(size)
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect()
    }

    #[test]
    fn the_reply_is_set_in_and_the_work_that_produced_it_is_not() {
        // A turn is prose, calls and notices stacked in one column, and they
        // used to be told apart only by being read. The reply opens with the
        // same `●` a call does, which puts its words in the column a call's
        // result hangs in — the eye finds what the model *said* in the same
        // shape it finds what the model *ran*, and neither is mistaken for the
        // other.
        //
        // The other half is the assertion that keeps this from becoming "mark
        // everything": the notice and the user's own bar stay flush left. A rule
        // applied by kind has to be checked on the kinds it excludes, or it is
        // indistinguishable from a rule applied to nothing.
        let rows = stream_rows(&fed(), (64, 30));
        let prose = rows
            .iter()
            .find(|r| r.contains("Fixed it"))
            .expect("the model's reply");
        assert!(
            prose.starts_with(&format!("{} ", Caps::default().g(Glyph::ToolMark))),
            "the reply does not open with a mark: {prose:?}"
        );
        // Against the tool call's own gutter rather than against `2`: what is
        // being claimed is that the two line up, and a bare number here would
        // keep passing if both moved apart together.
        let gutter = rows
            .iter()
            .find(|r| r.contains('⎿'))
            .expect("a tool result hanging in its gutter");
        assert_eq!(
            words(prose),
            words(gutter),
            "the reply and the tool result do not start in the same column: \
             {prose:?} vs {gutter:?}"
        );
        assert!(
            words(prose) > 0,
            "the reply's words start at the margin, so nothing is set in: {prose:?}"
        );

        let call = rows
            .iter()
            .find(|r| r.contains("cd /Users"))
            .expect("a tool call");
        assert_eq!(leading(call), 0, "the call was set in too: {call:?}");

        let notice = rows
            .iter()
            .find(|r| r.contains("rate limited"))
            .expect("a notice");
        assert_eq!(leading(notice), 0, "the notice was set in too: {notice:?}");

        let bar = rows
            .iter()
            .find(|r| r.contains("fix the build"))
            .expect("what was asked");
        assert_eq!(
            leading(bar),
            0,
            "the user's bar was set in, which eats its point: {bar:?}"
        );
    }

    #[test]
    fn a_reply_sits_in_the_same_column_while_it_streams_and_once_it_is_done() {
        // A reply has two renderers, and the mark and its margin have to be in
        // neither of them. While the answer is arriving it is drawn by the live
        // cache, which parses the text itself (`markdown::render_settled`) so
        // that a frame costs the new text rather than the whole answer; once the
        // turn ends the same block is settled and drawn by `Content::lines`. A
        // margin put inside the block — the obvious place, and the first one I
        // wrote — reaches the second and not the first, and the prose then jumps
        // two cells sideways at the exact moment the answer lands.
        //
        // `set_in` is called where both renderers return, which is what this
        // asserts. It is also why the mark is not `ModelSaid`'s business.
        let facts = conformance::facts();
        // Through the last chunk: the text block is open and still growing.
        let streaming = host();
        for f in &facts[..6] {
            streaming.absorb(f);
        }
        // …and one fact further, which is the message that settles it.
        let done = host();
        for f in &facts[..8] {
            done.absorb(f);
        }

        let live = stream_rows(&streaming, (64, 30));
        let settled = stream_rows(&done, (64, 30));
        let arriving = live
            .iter()
            .find(|r| r.contains("Looking."))
            .expect("the answer as it arrives");
        let landed = settled
            .iter()
            .find(|r| r.contains("Looking."))
            .expect("the same answer once it is done");

        assert!(
            arriving.starts_with(&format!("{} ", Caps::default().g(Glyph::ToolMark))),
            "a streaming reply does not open with a mark: {arriving:?}"
        );
        assert!(
            words(arriving) > 0,
            "a streaming reply's words start at the margin: {arriving:?}"
        );
        assert_eq!(
            arriving, landed,
            "the reply moved when it settled: {arriving:?} became {landed:?}"
        );
    }

    #[test]
    fn the_mark_opens_the_reply_once_and_the_rows_under_it_line_up() {
        // The mark is the *block's* — it says "here is a piece of this turn" —
        // so it goes on the block once. On every row it would read as a list of
        // separate things; on none of the rows after the first, the
        // continuation drifts a column left. So this is a test of the shape and
        // not only of the first row.
        let h = host();
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: "第一行很长很长很长很长很长很长很长很长\n第二行".into(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
        });
        let rows = stream_rows(&h, (40, 20));
        let mark = format!("{} ", Caps::default().g(Glyph::ToolMark));
        let first = rows
            .iter()
            .find(|r| !r.trim().is_empty())
            .expect("the reply");
        assert!(
            first.starts_with(&mark),
            "the reply does not open with a mark: {first:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains(&mark)).count(),
            1,
            "the mark belongs to the block, not to each of its rows: {rows:?}"
        );
        // `words` is the mark's width on the first row and the leading blanks on
        // the rest, so one equality says both halves of "it lines up".
        let column = crate::width::str_width(&mark);
        for row in rows.iter().filter(|r| !r.trim().is_empty()) {
            assert_eq!(
                words(row),
                column,
                "a row of the reply is not in the reply's column: {row:?}"
            );
        }
    }

    #[test]
    fn the_reply_leaves_exactly_the_room_its_mark_takes() {
        // Two numbers have to be the same one: the width the answer is rendered
        // into, and the width the mark it opens with actually occupies. If the
        // renderer is handed *more* room than the mark takes, the row the mark
        // is prepended to is one cell too wide and its last character is cut
        // off — the failure is silent, because a truncated character is not a
        // crash and the arithmetic on both sides still agrees with itself.
        //
        // `inset` is what makes them one fact, and this is what says so. The
        // assertion is against the glyph rather than against `inset`, so an
        // `inset` that stopped reading the mark would be caught rather than
        // agreeing with itself.
        let mark = format!("{} ", Caps::default().g(Glyph::ToolMark));
        assert_eq!(
            inset("assistant"),
            crate::width::str_width(&mark) as u16,
            "the reply is set in by something other than the mark it draws"
        );

        // And the consequence: a reply that fills its room exactly keeps every
        // character, rather than losing the one the mark's second cell eats.
        const W: u16 = 40;
        let room = (W - inset("assistant")) as usize;
        let text = "字".repeat(room / 2);
        let h = host();
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: text.clone(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
        });
        let drawn: String = stream_rows(&h, (W, 20))
            .iter()
            .map(|r| {
                r.trim_start_matches(' ')
                    .trim_start_matches(&mark)
                    .to_string()
            })
            .filter(|r| !r.trim().is_empty())
            .collect();
        assert_eq!(
            drawn.matches('字').count(),
            text.matches('字').count(),
            "the reply lost characters to the width it was drawn at: {drawn:?}"
        );
        for row in stream_rows(&h, (W, 20)) {
            assert!(
                crate::width::str_width(&row) <= W as usize,
                "a row was drawn wider than the screen: {row:?}"
            );
        }
    }

    #[test]
    fn a_wrapped_reply_is_measured_at_the_width_it_is_drawn_at() {
        // The scroll bound is "what there is to read, minus what is already on
        // screen", so it and the frame have to be the same number. A reply that
        // is measured at the full width and drawn narrower by the mark's two
        // cells wraps into more rows than were counted — and the difference is
        // exactly the rows at the top of the reply that the limit then says do
        // not exist.
        //
        // The length *is* the test. Two cells of wrap is the whole difference
        // between the widths, so a fixture that wraps the same either way would
        // pass against the bug. These are full-width characters because the
        // arithmetic has to be exact: 320 cells is eight rows of 40 and nine of
        // 38, which is the one row that tells the two widths apart.
        const CELLS: usize = 320;
        let h = host();
        let long = "这".repeat(CELLS / 2);
        assert_eq!(
            crate::width::str_width(&long),
            CELLS,
            "the fixture is not the width this test's arithmetic is written against"
        );
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: long,
            reasoning: String::new(),
            tool_calls: Vec::new(),
        });
        // What the painter actually draws, counted off a screen tall enough to
        // hold the whole reply — so this is the frame's number, not the sum's.
        let drawn = stream_rows(&h, (40, 40))
            .iter()
            .filter(|r| !r.trim().is_empty())
            .count();
        assert!(
            drawn > 8,
            "the fixture wraps into {drawn} rows; this test needs more than the \
             eight a 40-cell measure gives it"
        );
        assert_eq!(
            h.stream_height(40),
            drawn,
            "the sum says there are {} rows to read and the frame drew {drawn}: \
             the reply was measured at the full 40 cells instead of the 38 it \
             is set in by, and the rows that difference makes are the rows at \
             the top of it that cannot be scrolled to",
            h.stream_height(40),
        );

        // And the consequence, rather than only the arithmetic: scrolled as far
        // back as the bound allows, the oldest row of the reply is on screen —
        // with the mark on it, which is the row that says the wrap started where
        // it was measured rather than a row below it.
        let size = (40, 8);
        let limit = h.scroll_limit(size, &Moment::default());
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
        let rows = stream_rows(&h, size);
        assert!(
            rows[0].starts_with(&format!("{} 这", Caps::default().g(Glyph::ToolMark))),
            "scrolled all the way back and the reply is not what is on screen: {:?}",
            &rows[..3]
        );
    }

    #[test]
    fn the_closing_rule_has_a_row_of_air_on_both_sides() {
        // The separator is the one row that is *about* the transcript: pressed
        // against the prose above and the next question below, it reads as one
        // more line of the answer instead of a boundary between two turns.
        let h = fed();
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        let rule = rows
            .iter()
            .position(|r| r.contains("完成") && r.contains('─'))
            .expect("the first turn's closing rule");
        // The prose it closes, a blank, the rule, a blank, the next question.
        assert!(
            rows[rule - 2].contains("Fixed it") && rows[rule + 2].contains("now break it"),
            "the rule is not between the two turns: {:?}",
            &rows[rule - 2..=rule + 2]
        );
        assert!(
            rows[rule - 1].trim().is_empty(),
            "no blank between the prose and the rule: {:?}",
            &rows[rule - 2..=rule]
        );
        assert!(
            rows[rule + 1].trim().is_empty(),
            "no blank between the rule and the next question: {:?}",
            &rows[rule..=rule + 2]
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
    fn a_tip_is_said_for_a_moment_and_moves_nothing() {
        // What the reserved row is for. Saying something is not an event in the
        // conversation — the whole reason it is not a block is that a block for
        // "已复制" pushed every row of the conversation up one.
        use crate::modules::{live, tip};
        use crate::moment::{Timestamp, NOTICE_MS};
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

        let resting = h.compose((60, 12));
        let field = resting.part("input").expect("the field").rect;
        let stream = resting.part("stream").expect("the words").rect;

        // The clock is the host's: the expiry is stamped from the reading the
        // frame is drawn from, and the module that draws it is handed the
        // result rather than asking a clock of its own.
        h.moment.write().unwrap().now = Timestamp::millis(1_000);
        h.say("已复制到剪贴板", false);
        let said = h.compose((60, 12));

        let tip: String = said
            .part("tip")
            .expect("the reserved row")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(tip.contains("已复制到剪贴板"), "the row says it: {tip:?}");
        assert_eq!(
            said.part("input").expect("the field").rect,
            field,
            "saying something does not move the field"
        );
        assert_eq!(
            said.part("stream").expect("the words").rect,
            stream,
            "and takes no row from the conversation"
        );
        assert!(
            said.part("stream")
                .expect("the words")
                .lines
                .iter()
                .all(|l| !l.plain().contains("已复制")),
            "and the conversation is not told at all"
        );

        // The reading the loop reaches three seconds later: gone, with nobody
        // having had to clear it.
        h.moment.write().unwrap().now = Timestamp::millis(1_000 + NOTICE_MS);
        let after = h.compose((60, 12));
        assert!(
            after
                .part("tip")
                .expect("the row is still reserved")
                .lines
                .iter()
                .all(|l| l.plain().trim().is_empty()),
            "a tip expires on its own"
        );
        assert_eq!(
            after.part("input").expect("the field").rect,
            field,
            "and nothing moved when it did"
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
        h.toggle_block(id, kind);
        let folded = h.compose(size).rows().join("\n");
        assert_ne!(before, folded, "clicking a block changed nothing");
        h.toggle_block(id, kind);
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
        h.toggle_block(id, kind);
        let open = h.compose(size).rows().join("\n");
        assert!(
            open.contains("hmm"),
            "the click showed the working:\n{open}"
        );

        h.toggle_block(id, kind);
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
    fn folding_one_block_moves_its_run_and_nothing_else() {
        // The pointing gesture is still the narrow one: ctrl-t moves every tool
        // call in the transcript, and a click must not. What it does move is the
        // run the clicked call belongs to — a lid that says `2 个工具` and then
        // hands over one of them is a lie about what was behind it.
        let h = fed();
        let size = (80, 40);
        let _ = h.compose(size);
        let calls: Vec<_> = {
            let stream = h.stream.read().unwrap();
            stream
                .slots()
                .iter()
                .map(|s| (s.block().id, s.block().kind()))
                .filter(|(_, k)| *k == "tool_call")
                .collect()
        };
        // The corpus has two runs, separated by the notice and the injection
        // between them, so the second is a different run from the first.
        assert!(calls.len() >= 3, "need two separate runs");
        let folded = |id| {
            h.presentation
                .read()
                .unwrap()
                .is_block_folded(id, "tool_call")
        };
        let untouched = calls.last().expect("a call in the second run").0;
        let was = folded(untouched);

        h.toggle_block(calls[0].0, "tool_call");

        assert_eq!(
            folded(calls[0].0),
            !was,
            "the call that was clicked did not move"
        );
        assert_eq!(
            folded(calls[1].0),
            !was,
            "its run-mate stayed behind, so the lid would lie"
        );
        assert_eq!(
            folded(untouched),
            was,
            "a call in another run moved too: that is the keyboard gesture"
        );
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
    fn the_environments_own_injections_are_off_the_screen_and_a_peers_is_not() {
        // The corpus carries both, and they arrive as the same `SessionEvent` with
        // a different `origin`. That is the pair this test exists for: if the two
        // were keyed alike, hiding the reminder would hide the report, and the
        // team's answer would be gone from the transcript with nothing saying so.
        let h = fed();
        let screen = h.compose((80, 40)).rows().join("\n");

        assert!(
            !screen.contains("system-reminder") && !screen.contains("[reminder]"),
            "the reminder is on screen:\n{screen}"
        );
        assert!(
            screen.contains("sessions are made in agent.rs"),
            "a teammate's report is an answer, and it is gone:\n{screen}"
        );

        // And back, one step at a time — the hidable cycle, same as reasoning: a
        // lid naming the kind, then the text, then away again. The group gesture
        // is `/showinject` with no argument.
        h.presentation
            .write()
            .unwrap()
            .toggle_many(crate::content::ENVIRONMENTAL_INJECTIONS.to_vec());
        let lid = h.compose((80, 40)).rows().join("\n");
        assert!(
            lid.contains("[reminder]"),
            "the group gesture did not put a lid on it:\n{lid}"
        );
        assert!(
            !lid.contains("keep going"),
            "a lid spends a row, not the text:\n{lid}"
        );
        assert!(
            !lid.contains("[memory]") && !lid.contains("[continuation]"),
            "a kind with nothing on screen grew a lid:\n{lid}"
        );

        h.presentation
            .write()
            .unwrap()
            .toggle_many(crate::content::ENVIRONMENTAL_INJECTIONS.to_vec());
        let open = h.compose((80, 40)).rows().join("\n");
        assert!(
            open.contains("keep going"),
            "expanding shows what it said:\n{open}"
        );

        // Round again, back to where the screen started.
        h.presentation
            .write()
            .unwrap()
            .toggle_many(crate::content::ENVIRONMENTAL_INJECTIONS.to_vec());
        assert_eq!(
            h.compose((80, 40)).rows().join("\n"),
            screen,
            "the cycle does not come back round"
        );
    }

    #[test]
    fn the_slash_menu_floats_over_the_layout_and_moves_nothing() {
        // 「上拉，盖在上面，不改变其它组件的大小」. The menu is drawn as a part
        // after the layout has been resolved, so opening it must not move a
        // single rect: the conversation keeps its rows, the field keeps its
        // height, the status bar stays put. It covers what is above the field
        // instead of taking room from it.
        let h = fed();
        let size = (80, 24);
        let before = h.compose(size);
        let stream = before.part("stream").expect("the conversation").rect;
        let field = before.part("input").expect("the field").rect;
        let status = before.part("status").expect("the status line").rect;

        h.set_menu(vec![
            ("help".into(), "看命令".into()),
            ("compact".into(), "压缩上下文".into()),
        ]);
        let after = h.compose(size);

        let menu = after.part("menu").expect("the menu is on screen");
        assert_eq!(
            after.part("stream").unwrap().rect,
            stream,
            "the conversation was resized to make room for the menu"
        );
        assert_eq!(
            after.part("input").unwrap().rect,
            field,
            "the field was resized to make room for the menu"
        );
        assert_eq!(after.part("status").unwrap().rect, status);
        assert_eq!(
            menu.rect.w, field.w,
            "the menu lines up with the field it rose out of"
        );
        assert_eq!(
            menu.rect.bottom(),
            field.y,
            "the menu sits directly against the field's top rule, its margin row included"
        );
        assert!(
            menu.rect.y < field.y,
            "the menu rose into the space above the field"
        );
        let drawn = &menu.lines;
        assert!(
            drawn.iter().any(|l| l.plain().contains("/help")),
            "the menu's own contents are drawn: {drawn:?}"
        );
        // The point of it being a panel rather than a list of words: every row
        // is filled to the rect with the panel's background, so what it covers
        // is covered. A row of text spans that stopped at the last word would
        // let the conversation show through on the right.
        let want = Some(crate::frame::Color::role(crate::theme::Role::PanelBg));
        for (i, line) in drawn.iter().enumerate() {
            assert_eq!(
                line.width(),
                menu.rect.w as usize,
                "menu row {i} does not fill the panel: {line:?}"
            );
            let bg = menu
                .lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.style.bg).collect::<Vec<_>>())
                .nth(i)
                .unwrap();
            assert!(
                line.spans.iter().all(|s| s.style.bg == want),
                "menu row {i} has cells with no background ({bg:?}): {line:?}"
            );
        }

        h.set_menu(Vec::new());
        assert!(
            h.compose(size).part("menu").is_none(),
            "closing the menu takes its part away"
        );
    }

    #[test]
    fn the_context_menu_floats_over_the_layout_and_moves_nothing() {
        // The same rule the slash menu follows, and for the same reason: a
        // panel that resizes the conversation as it appears is worse than none.
        // It also has to sit *under the pointer*, which is the difference
        // between this one and the slash menu.
        let h = fed();
        let size = (80, 24);
        let before = h.compose(size);
        let stream = before.part("stream").expect("the conversation").rect;
        let field = before.part("input").expect("the field").rect;

        h.open_context_menu(
            (6, field.y + 1),
            vec![
                crate::menu::Item::new("copy", "复制全文"),
                crate::menu::Item::new("paste", "粘贴"),
            ],
        );
        let after = h.compose(size);

        assert_eq!(
            after.part("stream").unwrap().rect,
            stream,
            "the conversation was resized to make room for the menu"
        );
        assert_eq!(
            after.part("input").unwrap().rect,
            field,
            "the field was resized to make room for the menu"
        );
        let menu = after.part("context-menu").expect("the menu is on screen");
        assert_eq!(menu.rect.x, 6, "it opens at the cell it was asked for");
        assert_eq!(
            menu.rect.y,
            field.y + 1,
            "its first item sits under the pointer"
        );
        let drawn: Vec<String> = menu.lines.iter().map(|l| l.plain()).collect();
        assert!(
            drawn.iter().any(|l| l.contains("复制全文")),
            "the menu's own items are drawn: {drawn:?}"
        );

        assert!(h.close_context_menu(), "closing says there was one");
        assert!(
            h.compose(size).part("context-menu").is_none(),
            "closing the menu takes its part away"
        );
        assert!(
            !h.close_context_menu(),
            "closing again says there was nothing to close"
        );
    }

    #[test]
    fn the_context_menu_takes_its_own_keys_and_gives_them_back_when_it_closes() {
        use crate::surface::KeyPress;
        let h = fed();
        assert!(
            h.context_menu_key(KeyPress::plain(crate::surface::Key::Down))
                .is_none(),
            "with nothing open the menu claims no keys"
        );

        h.open_context_menu(
            (0, 0),
            vec![
                crate::menu::Item::new("copy", "复制全文"),
                crate::menu::Item::new("send", "发送"),
            ],
        );
        let step = h
            .context_menu_key(KeyPress::plain(crate::surface::Key::Down))
            .expect("an open menu claims the key");
        assert_eq!(step, crate::menu::Step::Stay, "moving keeps it open");
        assert!(h.context_menu_open());

        let step = h
            .context_menu_key(KeyPress::plain(crate::surface::Key::Enter))
            .expect("still open");
        assert_eq!(step, crate::menu::Step::Picked("send".into()));
        assert!(
            !h.context_menu_open(),
            "picking an item closes the menu — it does not stay up over the answer"
        );

        // And the keyboard is the screen's again, not the closed menu's.
        h.open_context_menu((0, 0), vec![crate::menu::Item::new("copy", "复制全文")]);
        assert_eq!(
            h.context_menu_key(KeyPress::plain(crate::surface::Key::Esc)),
            Some(crate::menu::Step::Dismissed)
        );
        assert!(!h.context_menu_open());
    }

    #[test]
    fn a_press_outside_the_context_menu_closes_it_without_choosing() {
        use crate::host::ContextClick;
        let h = fed();
        let field = h.compose((80, 24)).part("input").unwrap().rect;
        h.open_context_menu(
            (6, field.y),
            vec![crate::menu::Item::new("copy", "复制全文")],
        );

        // Far from the menu: the caller is told it was not a choice, and the
        // menu has been put away so the press can mean whatever it meant.
        assert_eq!(
            h.context_menu_click(0, 0, (80, 24)),
            ContextClick::Outside,
            "a press elsewhere is not a choice off the menu"
        );
        assert!(!h.context_menu_open());

        // With nothing open, the menu has no opinion about the pointer at all.
        assert_eq!(h.context_menu_click(0, 0, (80, 24)), ContextClick::NotOpen);
    }

    #[test]
    fn a_press_on_a_row_picks_that_row() {
        use crate::host::ContextClick;
        let h = fed();
        let field = h.compose((80, 24)).part("input").unwrap().rect;
        h.open_context_menu(
            (6, field.y),
            vec![
                crate::menu::Item::new("copy", "复制全文"),
                crate::menu::Item::new("send", "发送"),
            ],
        );
        let rect = h.compose((80, 24)).part("context-menu").expect("open").rect;
        assert_eq!(
            h.context_menu_click(rect.x, rect.y + 1, (80, 24)),
            ContextClick::Picked(crate::menu::Step::Picked("send".into())),
            "the row under the pointer is the row that is chosen"
        );
        assert!(!h.context_menu_open());
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
