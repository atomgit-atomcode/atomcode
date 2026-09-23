//! The host: the four things no module may own.
//!
//! It holds the surface, the layout, focus, and the redraw cycle — and it knows
//! **no module by name**. Composition is: walk the region tree for rects, look
//! each leaf up in the registry, ask it to draw into its rect, check nothing
//! spilled. That last step is the pixel-level verdict on spatial
//! composability, and it runs in every frame in debug builds.

use std::sync::{Arc, Mutex, RwLock};

use atomcode_harness::session::{LoggedEvent, SessionEvent};

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

/// How much of a tool call's output the screen draws.
///
/// A third axis, and deliberately not a fourth [`Showing`]: that one says how
/// much of a *block* is drawn, this one says how a run of blocks is drawn
/// together. They multiply — a call can be folded (one row) and a run of them
/// merged (one lid for the lot) or not (one row each) — so folding the second
/// question into the first would make two of the four combinations
/// unspellable, which is exactly the state this replaces.
///
/// The middle two are the ones that used to be indistinguishable. `Folded`
/// meant *both* "draw each call as one row" and "merge a run into one lid", so
/// a person who wanted the first got the second and there was no way to ask for
/// either on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolOutput {
    /// Every call in full — the default, and what `b5006d77` settled on.
    #[default]
    Full,
    /// Expanded, but only just: the first and last twenty rows of each call,
    /// a muted `已折叠 N 行，点击展开` between them. A call that fits is
    /// drawn whole; one that does not is previewed, and clicking it opens it
    /// in full.
    Head,
    /// One summary row per call, no merging. A run of four is four rows.
    Each,
    /// One lid for a run of consecutive calls, however many there are.
    ///
    /// A lid is three rows (`● N 个工具`, the last call's head, its result), so
    /// it costs *more* than `Each` under three calls. A person picks it knowing
    /// that; the screen does not second-guess them with a threshold, which
    /// would make the count on the lid depend on the count in it.
    Group,
}

/// How many rows of an expanded call [`ToolOutput::Head`] keeps at each end.
const HEAD_ROWS: usize = 20;

/// The row-count an expanded call draws in [`ToolOutput::Head`] once it is
/// long enough to fold: the head rows, the fold note, the tail rows.
const HEAD_TOTAL: usize = HEAD_ROWS * 2 + 1;

/// How many rows a call previewed at [`ToolOutput::Head`] draws.
fn head_rows(full: usize) -> usize {
    if full <= HEAD_TOTAL {
        full
    } else {
        HEAD_TOTAL
    }
}

/// The shape one tool call is drawn in. See [`Presentation::tool_show`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolShow {
    /// The whole call, commands and results.
    Full,
    /// The first and last [`HEAD_ROWS`] rows with a muted fold note between.
    Preview,
    /// One summary row.
    Folded,
}

/// Which blocks are shown how. Kept here, keyed by kind, rather than on the
/// block — which is what makes "folding does not change content" structural.
pub struct Presentation {
    /// How each kind is shown right now. A kind nobody has touched is `Open`.
    by_kind: Vec<(&'static str, Showing)>,
    /// How much of a tool call's output is drawn. Not in `by_kind`, because it
    /// is not a `Showing`: it says how a *run* of calls is drawn, where
    /// `by_kind` says how one block is. See [`ToolOutput`].
    tool_output: ToolOutput,
    /// Blocks folded or unfolded by hand, overriding the default for their
    /// kind.
    ///
    /// Per block, because a click is about *this* tool call. Folding every tool
    /// call in the transcript because one was clicked is a different gesture,
    /// and it already has a key (ctrl-t) — a click that did it would be a click
    /// that changed six other things the person was looking at.
    by_block: std::collections::HashMap<BlockId, bool>,
    /// Tool calls hand-opened to the full drawing while the mode is
    /// [`ToolOutput::Head`]. Head previews every call; this is the set that
    /// said "but this one, whole". Cleared with the rest of the per-block
    /// state on a mode change — a statement about one call does not survive
    /// into a mode that decides for every call.
    full_open: std::collections::HashSet<BlockId>,
    /// Tool calls collapsed automatically the moment they finished — the
    /// "expanded while running, one row once done" default. Separate from
    /// `by_block` (a hand fold) so it survives a mode change: `ctrl-t` clears the
    /// hand state, but the auto-fold is a property of the *default* view, so a
    /// full cycle of the modes comes back to exactly the same screen. Consulted
    /// only in [`ToolOutput::Full`] (the default); the compact modes decide for
    /// themselves. Cleared with the stream, and rebuilt as results re-arrive.
    auto_folded: std::collections::HashSet<BlockId>,
    /// The turns a person **cancelled** and asked to have undone: their blocks
    /// are drawn as one dim line. Here rather than only on the moment because
    /// the row count keys on this struct's revision, and folding a turn changes
    /// what a frame counts.
    undone: std::collections::BTreeSet<u64>,
    /// The turns a **rewind** took back: their blocks are not drawn at all.
    ///
    /// A different answer from `undone`, for a different question. A cancelled
    /// turn is one somebody stopped halfway, and what it got done before they
    /// stopped is still worth reading — dim, because the model no longer sees
    /// it. A rewound turn is one they said should not have happened; leaving it
    /// on screen is leaving a conversation whose visible half disagrees with
    /// the model's. The `Rewound` block itself stays (it is `always_open`), so
    /// the history says what happened rather than quietly losing a stretch of
    /// itself.
    rewound: std::collections::BTreeSet<u64>,
    /// Bumped by every change above. See `host::row_index` for why the row
    /// count keys on this rather than being invalidated by hand: folding a
    /// kind, folding one block and hiding a kind all change what the frame
    /// counts, and a caller that forgot one of them would keep a count of a
    /// screen nobody is looking at.
    revision: u64,
}

impl Presentation {
    /// How the screen opens: reasoning and the environment's own injections
    /// away, tool calls shown in full.
    ///
    /// Both of the first two are the working rather than the answer, and a
    /// transcript is read for the answer. A tool call is the exception and is
    /// drawn whole, because *what was run* is part of that answer: someone
    /// glancing at a transcript wants to know that a file was read, not what
    /// the model was thinking while it read it.
    ///
    /// How much of a tool call is drawn is [`ToolOutput`]'s question, not this
    /// one's — `by_kind`'s entry for `tool_call` is here for the kinds' sake
    /// (it is what `is_hidden` reads) and no longer decides how the run is
    /// drawn.
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
            // Tool calls open by default, each shown in full (`● ReadFile(name)`
            // over `⎿ …`), the way the reference does it: expanding a call is how
            // a reader sees what actually ran, and a folded lid over a single call
            // hid its result behind a clip. A run can still be folded by hand.
            ("tool_call", Showing::Open),
        ];
        by_kind.extend(
            crate::content::ENVIRONMENTAL_INJECTIONS
                .iter()
                .map(|kind| (*kind, Showing::Hidden)),
        );
        Self {
            by_kind,
            tool_output: ToolOutput::default(),
            by_block: std::collections::HashMap::new(),
            full_open: std::collections::HashSet::new(),
            auto_folded: std::collections::HashSet::new(),
            undone: std::collections::BTreeSet::new(),
            rewound: std::collections::BTreeSet::new(),
            revision: 0,
        }
    }

    /// Whether this turn was cancelled and undone — drawn as one dim line.
    pub fn is_undone(&self, turn: u64) -> bool {
        self.undone.contains(&turn)
    }

    /// Whether a rewind took this turn back — not drawn at all.
    pub fn is_rewound(&self, turn: u64) -> bool {
        self.rewound.contains(&turn)
    }

    /// The turns that were taken back, each kind in its own set. `true` when
    /// either changed.
    fn set_undone(
        &mut self,
        turns: std::collections::BTreeSet<u64>,
        rewound: std::collections::BTreeSet<u64>,
    ) -> bool {
        if self.undone == turns && self.rewound == rewound {
            return false;
        }
        self.undone = turns;
        self.rewound = rewound;
        self.bump();
        true
    }

    /// Every change that alters what a frame would count goes through this.
    ///
    /// One place rather than a bump at each writer: the writers are `set`,
    /// `set_block`, `toggle` and `toggle_many`, and the last two call the first
    /// two — so bumping here covers every route by construction, including the
    /// next one somebody adds.
    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// What the row count keys on. See `host::row_index`.
    pub fn revision(&self) -> u64 {
        self.revision
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
        self.bump();
    }

    pub fn is_folded(&self, kind: &str) -> bool {
        // A tool call's fold state is the mode's, not `by_kind`'s: the two
        // states of `Showing` cannot spell "one row each" apart from "one lid
        // for the run", and that distinction is the whole of [`ToolOutput`].
        // Head is not folded either — a preview is most of the drawing — so
        // only the two summary modes fold by default.
        if kind == "tool_call" {
            return matches!(self.tool_output, ToolOutput::Each | ToolOutput::Group);
        }
        self.showing(kind) == Showing::Folded
    }

    /// How much of one tool call the screen draws, the three shapes a call
    /// can take: whole, previewed at [`ToolOutput::Head`], or a one-row
    /// summary. `Full` and `Each`/`Group` spell two of these from the mode
    /// alone; Head spells its third per call, because the whole point of the
    /// mode is that most calls are previewed and one the reader opens is
    /// whole.
    ///
    /// A hand fold (`by_block`) wins over everything: it is what a click said
    /// about *this* call.
    /// Whether a finished tool call recedes to its one-row summary by default.
    ///
    /// The paint ([`tool_show`]) and the row count ([`is_block_folded`]) both
    /// ask, through this one predicate, so the two cannot drift: a call measured
    /// as one row and drawn as many is the ghost/overlap class of bug. Only the
    /// default [`ToolOutput::Full`] view auto-folds; the compact modes decide for
    /// every call themselves. A hand fold/unfold in `by_block` sits on top and is
    /// checked by each caller first, so it is not consulted here.
    ///
    /// [`tool_show`]: Self::tool_show
    /// [`is_block_folded`]: Self::is_block_folded
    fn auto_folds(&self, id: BlockId) -> bool {
        self.tool_output == ToolOutput::Full && self.auto_folded.contains(&id)
    }

    fn tool_show(&self, id: BlockId) -> ToolShow {
        if self.by_block.get(&id).copied().unwrap_or(false) {
            return ToolShow::Folded;
        }
        // A finished call recedes to one summary row in the default view — unless
        // the reader has spoken about it by hand (`by_block`: `unwrap_or(false)`
        // above returns only on a hand *fold*, so the guard here keeps a
        // hand-*opened* call, present with `false`, from being re-folded).
        if !self.by_block.contains_key(&id) && self.auto_folds(id) {
            return ToolShow::Folded;
        }
        match self.tool_output {
            ToolOutput::Full => ToolShow::Full,
            ToolOutput::Head => {
                if self.full_open.contains(&id) {
                    ToolShow::Full
                } else {
                    ToolShow::Preview
                }
            }
            ToolOutput::Each | ToolOutput::Group => ToolShow::Folded,
        }
    }

    /// Whether this call draws as a Head preview — the clipped form with the
    /// fold note — rather than whole or summarised. The row index and the
    /// painter both ask, so the count and the picture agree.
    fn previews(&self, id: BlockId, kind: &str) -> bool {
        kind == "tool_call" && self.tool_show(id) == ToolShow::Preview
    }

    /// How a run of tool calls is drawn. See [`ToolOutput`].
    pub fn tool_output(&self) -> ToolOutput {
        self.tool_output
    }

    /// Whether a run of consecutive calls merges into one lid.
    pub fn merges_tool_runs(&self) -> bool {
        self.tool_output == ToolOutput::Group
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
        if let Some(folded) = self.by_block.get(&id) {
            return *folded;
        }
        // A finished call recedes to one row in the default view; a hand fold
        // above already took precedence (`by_block.get` returns for any entry).
        if kind == "tool_call" && self.auto_folds(id) {
            return true;
        }
        self.is_folded(kind)
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
    ///
    /// A tool call is the one kind with four states to step through rather
    /// than two, and they are its own: `Full → Head → Each → Group → Full`. The
    /// order runs from most detail to least and then back, so a press always
    /// answers "less of this, please" until there is no less to show.
    pub fn toggle(&mut self, kind: &'static str) {
        if kind == "tool_call" {
            self.set_tool_output(match self.tool_output {
                ToolOutput::Full => ToolOutput::Head,
                ToolOutput::Head => ToolOutput::Each,
                ToolOutput::Each => ToolOutput::Group,
                ToolOutput::Group => ToolOutput::Full,
            });
            return;
        }
        let next = match self.showing(kind) {
            Showing::Hidden => Showing::Folded,
            Showing::Folded => Showing::Open,
            Showing::Open if hideable(kind) => Showing::Hidden,
            Showing::Open => Showing::Folded,
        };
        self.set(kind, next);
        self.by_block.clear();
    }

    /// How much of a tool call's output to draw, flatly.
    ///
    /// Flatly and not as a toggle, the way [`set_block`](Self::set_block) is: a
    /// slash command may name the state it wants, and a toggle would make
    /// `/tools group` mean something different depending on where it started.
    pub fn set_tool_output(&mut self, to: ToolOutput) {
        if self.tool_output == to {
            return;
        }
        self.tool_output = to;
        // The per-block choices go with it: a call folded by hand is a
        // statement about *that* call, and it would survive into a mode whose
        // whole point is to decide how every call is drawn. The Head mode's
        // hand-opened calls go too, for the same reason.
        self.by_block.clear();
        self.full_open.clear();
        self.bump();
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
        self.bump();
    }

    /// Collapse a block by default the moment a tool call finishes: it was
    /// expanded while running (so its command was in view), and once the result
    /// is in there is nothing to watch, so it recedes to one row.
    ///
    /// `auto_folded` is the *default* layer, distinct from `by_block`. A hand
    /// fold/unfold sits on top and wins at paint time while it is present (both
    /// [`tool_show`] and [`is_block_folded`] check `by_block` first). So the
    /// finished call is recorded here regardless of what the reader has said by
    /// hand: while their choice stands it is overruled, but once a mode cycle
    /// clears `by_block` the call falls back to its one-row default rather than
    /// springing open — the "a full cycle comes back to the same screen" property
    /// that `auto_folded` exists to keep, held for hand-touched calls too.
    ///
    /// [`tool_show`]: Self::tool_show
    /// [`is_block_folded`]: Self::is_block_folded
    pub fn fold_finished(&mut self, id: BlockId) {
        if self.auto_folded.insert(id) {
            self.bump();
        }
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
/// detail — `· 思考 3 行`, `● ReadFile(a.rs) · ok` — and a click on a lid has
/// exactly one meaning. Folding a thought was reachable only by ctrl-r, which
/// moves *every* thought in the transcript; the row itself answered nothing, so
/// pointing at a lid opens that lid and costs no other gesture. A press that
/// moves is still a selection, and ctrl-r still does them all at once.
const CLICKABLE: [&str; 2] = ["tool_call", "reasoning"];

/// How many rows the slash menu may take, margin aside.
///
/// Well short of the screen, so it reads as something that rose out of the
/// prompt rather than as a second transcript. It is a **window** rather than a
/// truncation — [`crate::menu::Slash::window`] keeps the lit row inside it and
/// scrolls by the least it can — so the cap costs no reachability, which is
/// what it did when the list was drawn from the top and simply cut off.
const MENU_ROWS: u16 = 10;

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
/// The turn's closing summary is a separator — `✻ Done · 3 轮 · 2 工具` — and gets
/// a row of air on both sides unconditionally. It is the one row that is *about*
/// the transcript rather than part of it, and pressed against the prose above
/// and the next question below it stops reading as a boundary and starts
/// reading as one more line of the answer.
///
/// The user's message opens a paragraph downwards: the answer starts under a
/// blank rather than directly under the bar, where it would read as the first
/// line of what was asked rather than as a reply to it. Nothing is needed above
/// it, because what is normally there is the closing summary — which carries
/// its own margin.
///
/// One definition, consulted by both the painter and the height the scroll is
/// measured against — the two have to agree or the last rows of a long
/// transcript become unreachable.
fn blank_between(upper: &str, lower: &str) -> bool {
    if upper == "turn_end" || lower == "turn_end" {
        return true;
    }
    // A user bar gets air on BOTH sides: after it (its answer starts fresh) and
    // before it (the bar is where a reader scans for "what did I ask", and it
    // should not butt up against the previous turn's last line or a compaction
    // notice — one blank row sets the question apart from what came before).
    if upper == "user" || lower == "user" {
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

/// 关掉 `stream_lines` 的二分跳转，退回从最新一块逐格走动。
///
/// 等价性棘轮的两条腿之一 —— 见 `host::tests::the_jump_draws_what_the_walk_draws`。
#[cfg(test)]
pub(crate) static NO_JUMP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
/// A block the reader cannot see is stepped over, not stopped at — a hidden
/// slot draws no rows, so it is not a seam in what was done: two calls with a
/// thought between them ran back to back, and the thought is not a reason to
/// spend a second `●` on them. Counting a hidden slot as the end of a run put
/// every call of a working session behind a lid of its own, because the model
/// thinks between calls — four calls that ran back to back, drawn as four rows
/// that each said `1`.
///
/// Anything the reader *can* see ends the run, and the model's own words
/// between two calls are the common one. A lid is drawn at the run's last
/// call, so a run drawn over its prose would put those words above the lid —
/// said, to all appearances, before any of the work the lid stands for. The
/// prose breaks the run precisely so it stays in the order it happened in.
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
    /// How many of them failed. Carried on the run rather than counted again
    /// where the lid is drawn: the lid only has the last call in hand, and the
    /// failures it has to report may all be in the calls before it.
    failed: usize,
    /// Whether the run is still the newest thing on screen: nothing visible
    /// follows its last call. A run followed by prose or a further turn is
    /// history and collapses to the count; a run at the visible end of the
    /// stream keeps drawing its last call, finished or not — the collapse is
    /// for history, and history begins when something visible comes after.
    live: bool,
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

fn lids(
    slots: &[crate::block::Slot],
    pres: &Presentation,
    activity: crate::moment::Activity,
) -> Lids {
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
        // Merging is asked of the mode, not inferred from the fold state: the
        // two were one condition while `Folded` meant both, and a person who
        // wanted one row per call got a lid for the run instead.
        if members.len() > 1 && all_folded && pres.merges_tool_runs() {
            let failed = members
                .iter()
                .filter(|m| {
                    slots[**m]
                        .block()
                        .content
                        .as_tool_call()
                        .is_some_and(|c| c.is_failed())
                })
                .count();
            // Live while the turn is still running and nothing visible follows
            // the run's last call. The turn ending collapses the lid even with
            // nothing after it: the count is the settled form, and a turn that
            // stopped leaves nothing in flight to keep drawing. A hidden slot
            // contributes no rows, so it does not end the run's tenure as the
            // newest thing on screen. `run_from` ended the run at `next`, so
            // everything from there on is what follows it.
            // Live only while NOTHING non-hidden follows the run — decided by the
            // block's EXISTENCE, not its current rendered height. Blocks only
            // accumulate, so "a non-hidden block follows" is monotonic and the fold
            // never flips back to live. The earlier `!lines(bare(1)).is_empty()`
            // check read the *following* block's height each frame, which a
            // streaming answer (and reasoning that is stripped and re-filled)
            // toggles empty↔non-empty — flipping `live` true↔false and re-expanding
            // an already-folded run every few frames: the tool-output flicker.
            let live = activity == crate::moment::Activity::Working
                && !slots[next..]
                    .iter()
                    .any(|s| !pres.is_hidden(s.block().kind()));
            let run = Run {
                last: *members.last().expect("a run has a first member"),
                count: members.len(),
                failed,
                live,
            };
            for m in members {
                runs[m] = Some(run);
            }
        }
        i = next;
    }
    Lids { runs }
}

/// How many rows slot `i` contributes, in the terms the frame counts.
///
/// `None` when it draws nothing at all — off-screen by kind, behind a lid that
/// is not its last member, or an empty block. The distinction matters to the
/// walk as well as to the sum: a slot that draws no rows is not a neighbour
/// either, so the blank rows around it stay where they were.
fn lid_row(
    lids: &Lids,
    slots: &[crate::block::Slot],
    i: usize,
    ctx: &crate::block::RenderCtx,
    b: &crate::block::Block,
    pres: &Presentation,
) -> Option<SlotRows> {
    let kind = b.kind();
    let room = ctx.width;
    if pres.is_hidden(kind) {
        return None;
    }
    // A turn a rewind took back is not drawn at all: the person said it should
    // not have happened, and a screen still showing it is a conversation whose
    // visible half disagrees with the model's. What *did* happen is the
    // `Rewound` block, which is `always_open` and so survives this.
    if pres.is_rewound(b.at.turn) && !b.content.always_open() {
        return None;
    }
    // A turn the person cancelled is one dim line, whatever its kind does and
    // whether or not it would have merged into a run: what it said happened, and
    // the model no longer sees it.
    if pres.is_undone(b.at.turn) && !b.content.always_open() {
        return Some(SlotRows {
            // Asked of the content, not assumed: the count and the picture have
            // to be the same answer. It was a constant `1` while every folded
            // block was one row; a call the model explained draws two, and a
            // `1` here would be the ghost/overlap class of bug — the scroll
            // counting one row under a block that wrote two.
            rows: b.content.summary_lines(ctx).len(),
            kind,
            lid: None,
            lid_failed: 0,
            lid_live: false,
            preview: false,
            folded: true,
            undone: true,
        });
    }
    match lids.at(i) {
        // The last member of a merged run draws the lid, which stands for the
        // whole run.
        Some(run) => Some(SlotRows {
            rows: lid_lines(slots, i, run.count, run.failed, run.live, room).len(),
            kind,
            lid: Some(run.count),
            lid_failed: run.failed,
            lid_live: run.live,
            preview: false,
            folded: false,
            undone: false,
        }),
        // An earlier member of a run: the lid at the end of it already drew.
        None if lids.covers(i) => None,
        // A call the Head mode previews: the full row count clipped to the
        // two ends plus the fold note. Measured through `rows_at` so the
        // clip and the painter's own drawing agree on where the ends fall.
        None if pres.previews(b.id, kind) => {
            let full = slots[i].rows_at(ctx).0;
            Some(SlotRows {
                rows: head_rows(full),
                kind,
                lid: None,
                lid_failed: 0,
                lid_live: false,
                folded: false,
                preview: full > HEAD_TOTAL,
                undone: false,
            })
        }
        None if !b.content.always_open() && pres.is_block_folded(b.id, kind) => Some(SlotRows {
            // Measured, not assumed — same reason as the undone branch above:
            // this is the number `stream_height` sums and the painter scrolls
            // by, and a folded call the model explained draws two rows.
            rows: b.content.summary_lines(ctx).len(),
            kind,
            lid: None,
            lid_failed: 0,
            lid_live: false,
            folded: true,
            preview: false,
            undone: false,
        }),
        None => Some(SlotRows {
            rows: slots[i].rows_at(ctx).0,
            kind,
            lid: None,
            lid_failed: 0,
            lid_live: false,
            folded: false,
            preview: false,
            undone: false,
        }),
    }
}

/// The one lid a merged run is drawn as, from its last call.
///
/// `failed` is counted over the whole run, not read off the last call: the lid
/// draws the last call's result and nothing else, so a run whose third call
/// failed and whose fourth succeeded would otherwise read as one that did not.
/// `live` is whether the run is still the newest thing on screen: a live lid
/// draws the call in flight, a settled one the count.
fn lid_lines(
    slots: &[crate::block::Slot],
    last: usize,
    count: usize,
    failed: usize,
    live: bool,
    w: u16,
) -> Vec<crate::frame::Line> {
    match slots[last].block().content.as_tool_call() {
        Some(call) => crate::content::ToolCallBlock::group_lines(call, count, failed, live, w),
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
/// visible block stops the run, a hidden one is stepped over — because a lid
/// that says `4 个工具` and then hands over three of them is a lie about what
/// was behind it.
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

/// One frame's split of a `Stream` region: the conversation's own rect, the
/// offset it is drawn at, and where each tail module landed.
///
/// The pane a `Stream` is given covers more than the blocks: the view modules
/// riding its tail draw inside it too, at the bottom, and the conversation's
/// rect is what is left once they have taken their rows. See
/// [`Host::pane_geometry`].
struct Pane {
    /// What the blocks are drawn into — the pane minus whatever tail rows the
    /// window still covers.
    block_rect: Rect,
    /// The offset `block_rect` is drawn at, which is `scroll` *less* the rows
    /// the tail took. Zero while any of the tail is still on screen.
    block_scroll: usize,
    /// `(id, rect)` per tail module that is at least partly visible, top to
    /// bottom. A module wholly above the fold is absent, which is what makes
    /// `part("stream.tail.live")` mean "on screen" rather than "declared".
    tail: Vec<(String, Rect)>,
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
    /// Where the question panel was drawn, so a click on it can find an answer.
    ///
    /// The panel's own rect and not a per-row table: the rows inside it are the
    /// answers plus the prompt's wrapped lines, and which screen row is which
    /// answer is a fact about how the prompt wrapped — the panel already knows,
    /// and a second copy here would be a second wrapping.
    ask: Option<Rect>,
    /// Where the team panel was drawn, so a press or the pointer on a row finds
    /// the agent it switches to.
    team: Option<Rect>,
    /// Where the settings panel was drawn, so a press or the pointer on a row
    /// finds the setting it changes.
    ///
    /// The rect the panel was **drawn** in, not one re-derived at the press: the
    /// panel rides the tail, so where it sits depends on how tall the modules
    /// below it turned out to be. The panel's own rect and not a per-row table,
    /// for the reason the question panel's is not one either — which screen row
    /// holds which setting is a fact about how the list was laid out at this
    /// width, and the panel already worked it out.
    settings: Option<Rect>,
    /// And the providers panel's, for the same reason.
    providers: Option<Rect>,
    /// And the plugins panel's, for the same reason.
    plugins: Option<Rect>,
    /// And the tools panel's.
    tools: Option<Rect>,
    /// And the rewind panel's.
    rewind: Option<Rect>,
    /// And the resume panel's.
    resume: Option<Rect>,
    /// Where the slash menu was drawn, so a press or the pointer on a row finds
    /// the command it is on.
    ///
    /// The rect the panel was **drawn** in, not one re-derived at the press:
    /// the menu hangs off the field's top edge and its window scrolls with the
    /// cursor, so a second computation here is a whole list answering to the
    /// wrong rows. The same rule the ask panel follows.
    menu: Option<Rect>,
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

    /// Where the question panel was put, when it was up.
    pub fn ask_rect(&self) -> Option<Rect> {
        self.ask
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
    ///
    /// A [`crate::menu::Slash`] and not a bare `Vec`: the list has a cursor
    /// now, and a cursor has to survive the redraws a keystroke causes. The
    /// host holds it for the same reason it holds the items — it is the thing
    /// that draws the panel, and what is pointed at is part of the picture.
    menu: RwLock<crate::menu::Slash>,
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
    /// The password a running process is blocked on, while one is being asked
    /// for. Beside `asks` because it is the same kind of thing — something
    /// outside this screen waiting on the person — and apart from it because a
    /// password must never become a fact the way an answer does. It is typed on
    /// the composer's line and takes the keyboard from everything while it is
    /// there; see [`crate::secret`].
    pub secrets: Arc<crate::secret::Secrets>,
    /// The API key being typed into the providers panel's form, while one is.
    ///
    /// Here rather than on the form for the reason `crate::secret` keeps a
    /// password out of `Moment`: the panel lives in the moment, the moment is
    /// cloned once a frame and readable by every module, and a credential has
    /// no business travelling that road. The form carries its *length*, which
    /// is all the dots a panel draws need. It is lent to
    /// `crate::providers::key` for one press and emptied whenever a form is
    /// left — see [`Host::providers_key`].
    providers_secret: Mutex<String>,
    /// The mounted cell-grid bitmaps. The host holds the table; a row writes
    /// through `RastersSvc`, and every frame takes a snapshot of it into
    /// `Moment` for the modules to draw. See `docs/adr/0027`.
    pub rasters: Arc<crate::raster::Rasters>,
    pub modules: Arc<Modules>,
    pub layout: Arc<crate::layout::Layout>,
    pub moment: RwLock<Moment>,
    pub presentation: RwLock<Presentation>,
    /// Frames composed so far. Only counted, not kept — the surface keeps them
    /// when it is the headless one.
    painted: Mutex<u64>,
    /// What was on screen last, so a click can be answered from it.
    hits: Mutex<Hits>,
    /// The box the conversation had in the last frame.
    ///
    /// Both dimensions, and **the stream's own**, not the screen's: a block is
    /// measured at the width it is drawn at, and under a layout that does not
    /// give the stream the whole screen (`wide`) the two are not the same. The
    /// height is what the tail has to share with the conversation.
    ///
    /// Kept because `absorb` runs between frames and has to measure in the same
    /// terms the frame did.
    last_room: Mutex<Rect>,
    /// Serialises "hold the reading still while this changes".
    ///
    /// The facts arrive on the emitter's thread and the activity on the event
    /// loop's, so a turn ending can commit a fact and flip the activity at the
    /// same moment. Both change how much there is to read and both compensate
    /// for it, and two compensations for one change is a screen that jumps by
    /// the difference. One gate, held across measure-change-measure, is what
    /// makes the pair add up to one.
    pin_gate: Mutex<()>,
    /// Rows per slot, and how many have been laid down before each one.
    ///
    /// **This is the frame's row sum, asked once.** Two walks need it —
    /// `stream_height_in`, which bounds the scroll, and `stream_lines`, which
    /// fills the viewport — and both used to rebuild it from scratch on every
    /// call, which is why one wheel notch cost 17.7ms on a 1330-slot session
    /// (measured, debug, 2026-09-15): `scroll_limit` 4.1ms + `compose` 13.7ms,
    /// both O(slots) and both re-deriving the same arithmetic.
    ///
    /// Keyed on `(stream revision, presentation revision, width)` rather than
    /// invalidated by hand. Those three are the whole of what a row count
    /// depends on, so "when is this stale" is a question the type answers
    /// instead of one somebody has to remember — and the two revisions are
    /// bumped by every writer, not by the call sites that happen to exist today.
    row_index: Mutex<RowIndex>,
}

/// The cached layout of the stream's rows, top to bottom.
///
/// Per slot, in slot order, with `None` for a slot that draws no rows. What
/// each entry holds is what a walk needs and what a walk would otherwise
/// recompute: how many rows the slot contributes, whether it is covered by a
/// merged lid (both the *last* slot of a run, which draws the lid, and its
/// earlier members, which draw nothing), and whether a blank row separates it
/// from the slot above.
struct RowIndex {
    /// What the measurements below were taken under. A different width, a
    /// different terminal, or a different idea of which blocks are folded,
    /// changes the answer for *every* slot — so those invalidate the lot.
    width: u16,
    /// The other half of the width's key. Without it a block whose row count
    /// depends on what the terminal can draw would keep answering with the old
    /// count while the painter drew the new one — a scroll bound that does not
    /// match the picture, which is the "one number, two answers" failure this
    /// table exists to avoid. See [`crate::block::ShapeCaps`].
    caps: crate::block::ShapeCaps,
    presentation: u64,
    /// Whether the turn was still running when this index was built. It is
    /// part of the key for the same reason the presentation revision is: a
    /// live run's lid draws the call in flight (two rows), and the turn
    /// ending collapses it to the count (one row) — an answer that changes
    /// for every slot at once, without any block or fold changing.
    activity: crate::moment::Activity,
    /// One entry per slot measured so far, in slot order. May be shorter than
    /// the stream while entries are being added at the end.
    measured: Vec<Measured>,
    /// `rows[i]` — see [`SlotRows`], `None` for a slot that draws nothing.
    rows: Vec<Option<SlotRows>>,
    /// How much a newest-first walk has accumulated once it has passed every
    /// slot from the end down to `i` — in **the walk's own terms**, seams and
    /// all: `rows_j` plus the seam `j` owns (the one between it and the next
    /// drawn slot newer).
    ///
    /// A suffix sum rather than a prefix sum, and that is the point. A prefix sum
    /// can say what is *above* a slot; only this can say what the walk will have
    /// in `skipped` when it gets there — and those two differ by exactly one
    /// seam per boundary, because the two walks disagree about which side of a
    /// seam owns it. Deriving the value from a prefix sum got that off by one and
    /// scrolled the first thing said off the top; storing the walk's own number
    /// cannot be off, because no second conversion is involved.
    ///
    /// Length `slots.len() + 1`, so `skip_from[n] == 0` is the reader at the
    /// bottom with nothing passed yet.
    skip_from: Vec<usize>,
    /// The sum over the whole stream, which is `before.last()` plus the last
    /// slot's own rows.
    total: usize,
}

/// One slot's measurement, and what it was measured for.
///
/// The tag is what makes the index incremental, which is the difference between
/// this helping and this hurting. A streamed answer commits a fact per token,
/// and each of those settles nothing and changes one slot — so a rebuild that
/// re-measured all 1330 slots would cost more than the walk it replaced. With
/// the tag, a rebuild re-measures the slot that changed and reuses the rest.
struct Measured {
    /// The block this was measured for, so a slot reused for a different block
    /// is not mistaken for the same one.
    id: crate::block::BlockId,
    /// Whether the block was settled when it was measured. A settled block's
    /// content cannot change, so its row count holds; a live one can grow, so
    /// it is measured again every time.
    settled: bool,
}

/// What one slot contributes to the row count — and everything the painter
/// would otherwise have to ask the block for.
///
/// The `kind`, `lid` and `folded` fields are not part of the sum. They are here
/// because the walk that fills the viewport asks the block the same questions
/// this table already asked while building it: what kind it is, whether a lid
/// covers it, whether it is drawn folded. Asking the block again per slot per
/// frame is four virtual calls and several string comparisons each — 1488 times
/// a frame — and on this session that was most of the 21ms `compose` cost at
/// the top of the scroll (measured, debug, 2026-09-15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SlotRows {
    /// Rows this slot draws. Zero means it is not a row of the transcript at
    /// all — hidden, or empty — and it is not a neighbour of anything either.
    rows: usize,
    /// The block's kind, kept so the walk does not call `kind()` again.
    kind: &'static str,
    /// Its turn was taken back: drawn as one dim line.
    undone: bool,
    /// The run this slot draws a lid for — its member count — or `None`.
    lid: Option<usize>,
    /// How many calls in that run failed. Carried beside `lid` for the same
    /// reason: the walk redraws the lid from this table, and the count it puts
    /// on the row has to be the one the measurement was taken with.
    lid_failed: usize,
    /// Whether that run was still the newest thing on screen when measured —
    /// the lid drawn as the call in flight rather than the count. Carried for
    /// the same reason: the walk redraws from this table, not from the run.
    lid_live: bool,
    /// Whether this slot is drawn as a Head preview — `rows` is the clipped
    /// count and the painter has to clip the drawing the same way. `false`
    /// when the whole call fits, so no note is drawn for it.
    preview: bool,
    /// Whether it is drawn as a one-row summary.
    folded: bool,
}

impl RowIndex {
    /// Where a newest-first walk may start, and the state it should start in.
    ///
    /// Returns `(start, skipped, below)`: the index to begin at, the rows a walk
    /// would have skipped by the time it got there, and the kind of the block
    /// below the window it would be carrying.
    ///
    /// **The arithmetic.** `total - before[i]` is the rows at or after slot `i`,
    /// which falls as `i` rises — so it is sorted, and the boundary between
    /// "wholly below the window" and "at least partly in it" is a partition
    /// point. A slot `i` with `total - before[i] <= scroll` is *certainly*
    /// skipped by the walk: its own rows plus everything newer already fits in
    /// the scrolled-past region. That makes `m`, the first such index, a safe
    /// place to stop skipping — so the walk can begin one slot earlier and let
    /// the ordinary loop decide the boundary exactly.
    ///
    /// What this buys: the walk used to reach the viewport one slot at a time
    /// from the newest block. On a 1570-slot session that was 5.25 of the 5.37ms
    /// a frame cost at the top of the scroll (release; 20 of 20.4ms in debug),
    /// for a window 24 rows tall.
    fn jump_to(&self, scroll: usize) -> (usize, usize, Option<&'static str>) {
        let n = self.rows.len();
        if n == 0 {
            return (0, 0, None);
        }
        // `skip_from` falls as the index rises, so "what a walk has passed by the
        // time it reaches here" is sorted and the boundary is a partition point:
        // the first slot a walk has already covered `scroll` rows before.
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.skip_from[mid] <= scroll {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        // One slot earlier than the first certainly-covered one, so the ordinary
        // loop decides the boundary exactly rather than this function guessing it.
        let start = lo.saturating_sub(1);
        let skipped = self.skip_from.get(start + 1).copied().unwrap_or(0);
        // What the walk would carry in `below`: the nearest slot above the
        // starting one that drew rows. A slot that draws nothing is not a
        // neighbour, so it is not an answer.
        let below = (start + 1..n).find_map(|i| {
            self.rows
                .get(i)
                .copied()
                .flatten()
                .filter(|e| e.rows > 0)
                .map(|e| e.kind)
        });
        (start, skipped, below)
    }
}

impl Host {
    pub fn new(modules: Arc<Modules>, layout: Region) -> Self {
        let layout_svc = Arc::new(crate::layout::Layout::new(layout));
        Self {
            stream: RwLock::new(Stream::new()),
            // Empty, like the module registry beside it. Command sets arrive as
            // rows (`crate::rows`); a Host that pre-filled this would make
            // `[[remove]] id = "tui-commands-session"` a lie.
            commands: Arc::new(crate::command::Commands::new()),
            menu: RwLock::new(crate::menu::Slash::default()),
            context_menu: RwLock::new(None),
            overlays: Arc::new(crate::overlay::Overlays::new()),
            asks: crate::ask::Asks::new(),
            secrets: crate::secret::Secrets::new(),
            providers_secret: Mutex::new(String::new()),
            rasters: Arc::new(crate::raster::Rasters::new()),
            modules: modules.clone(),
            layout: layout_svc.clone(),
            moment: RwLock::new(Moment::default()),
            presentation: RwLock::new(Presentation::default_folds()),
            painted: Mutex::new(0),
            hits: Mutex::new(Hits::default()),
            last_room: Mutex::new(Rect::default()),
            pin_gate: Mutex::new(()),
            row_index: Mutex::new(RowIndex {
                width: 0,
                caps: crate::block::ShapeCaps::of(&crate::caps::Caps::default()),
                presentation: 0,
                activity: crate::moment::Activity::default(),
                measured: Vec::new(),
                rows: Vec::new(),
                skip_from: Vec::new(),
                total: 0,
            }),
        }
    }

    /// Draw another session from here on (`docs/adr/0022` §6).
    ///
    /// Each session is a stream of its own, irreversible inside itself; moving
    /// to another is not a separator drawn into this one. So the stream, how its
    /// blocks are shown, the measurements taken of it and everything a module
    /// folded from it go, and the new session's facts start them over. The old
    /// stream is dropped rather than kept: the session it drew was replaced, and
    /// there is nothing to switch back to.
    /// Another session in place of this one: everything drawn goes, and what was
    /// waiting on this one — a question, the members — goes with it.
    pub fn switch_session(&self) {
        self.switch_view();
        // A question belongs to a turn of the session that asked it.
        self.asks.refuse_all();
        let mut m = self.moment.write().expect("moment poisoned");
        m.members.clear();
        m.team_cursor = None;
        // The keyboard went with the panel: the new session's team is not up
        // yet, and a leftover `true` here would route the first keys of the new
        // conversation into a panel that is not drawn.
        m.team_keyboard = false;
        // A name belongs to the session that was named. Left behind it would be
        // the previous conversation's name over the new one's composer — and
        // over the window, which is worse, because a window is what a person
        // picks between.
        m.title = None;
    }

    /// The screen, emptied to draw another agent of the same session — the lead
    /// or one of its members (`docs/adr/0023` §3). Their questions stay, and so
    /// does the team: only what was drawn from the one on screen goes.
    pub fn switch_view(&self) {
        *self.stream.write().expect("stream poisoned") = Stream::new();
        *self.presentation.write().expect("presentation poisoned") = Presentation::default_folds();
        *self.row_index.lock().expect("row index poisoned") = RowIndex {
            width: 0,
            caps: crate::block::ShapeCaps::of(&crate::caps::Caps::default()),
            presentation: 0,
            activity: crate::moment::Activity::default(),
            measured: Vec::new(),
            rows: Vec::new(),
            skip_from: Vec::new(),
            total: 0,
        };
        for producer in self.modules.producers() {
            producer.reset();
        }
        for id in self.modules.view_ids() {
            if let Some(view) = self.modules.view(id) {
                view.reset();
            }
        }
        let mut m = self.moment.write().expect("moment poisoned");
        m.activity = crate::moment::Activity::Idle;
        m.scroll = crate::moment::ScrollPos::BOTTOM;
        m.selection = None;
        m.turn_started = None;
        m.quiet_since = None;
        m.steering.clear();
        // Both belong to the view being left, not the one arriving: the `已中断`
        // note is about a turn this session stopped, and `last_sent` is what to
        // hand back on the next Escape. Carried across a `/clear` or a member
        // switch they would draw a phantom note over — and resend a foreign
        // prompt into — a conversation that never saw either.
        m.interrupted = false;
        m.last_sent = None;
    }

    /// Which team panel row a screen point is on, when it is a row that switches
    /// — read off the rect the panel was drawn in, like [`Host::answer_row_at`].
    pub fn team_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let rect = *self.hits.lock().expect("hits poisoned").team.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        crate::modules::team::target_at_line(&m, (y - rect.y) as usize)
    }

    /// Whether the team panel has the keyboard.
    ///
    /// Not "is a row lit": the pointer lights a row by being over it, and a
    /// panel that takes the keyboard because a mouse crossed it would eat what
    /// the person is typing. Only `Tab` sets this.
    pub fn team_focused(&self) -> bool {
        self.moment.read().expect("moment poisoned").team_keyboard
    }

    /// Whether plain Tab cycles the execution mode
    /// (`ui.mode_switch_key = "tab"`). Read live on each press, so a `/config`
    /// change takes effect without a restart — the same promise the setting's
    /// own `applies` makes.
    pub fn mode_switch_on_tab(&self) -> bool {
        self.moment
            .read()
            .expect("moment poisoned")
            .mode_switch_on_tab()
    }

    /// Give the team panel the keyboard, pointing at the agent on screen. `false`
    /// when there is no team to switch between.
    pub fn focus_team(&self) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let targets = crate::modules::team::targets(&m);
        if targets.is_empty() {
            return false;
        }
        let here = targets.iter().position(|s| *s == m.viewing).unwrap_or(0);
        m.team_cursor = Some(here);
        m.team_keyboard = true;
        true
    }

    /// Point at `row` of the team panel, clamped to the rows there are.
    ///
    /// Says which row would be taken, and nothing more: a pointer that moves
    /// across the panel does not take the keyboard with it.
    pub fn point_team_at(&self, row: usize) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let last = crate::modules::team::targets(&m).len().saturating_sub(1);
        let row = row.min(last);
        if m.team_cursor == Some(row) {
            return false;
        }
        m.team_cursor = Some(row);
        true
    }

    /// The session the panel is pointing at: the row the arrows are on while it
    /// has the keyboard, the row under the pointer otherwise. `None` when it is
    /// pointing at nothing.
    pub fn team_target(&self) -> Option<String> {
        let m = self.moment.read().expect("moment poisoned");
        let cursor = m.team_cursor?;
        crate::modules::team::targets(&m).get(cursor).cloned()
    }

    /// Move the team panel's pointer by `delta` rows, clamped.
    pub fn move_team_by(&self, delta: i32) -> bool {
        let cur = self
            .moment
            .read()
            .expect("moment poisoned")
            .team_cursor
            .unwrap_or(0) as i32;
        self.point_team_at((cur + delta).max(0) as usize)
    }

    /// Hand the keyboard back to the composer. The row it was on stays lit if
    /// the pointer is still over it — pointing and typing are different things.
    pub fn unfocus_team(&self) {
        self.moment.write().expect("moment poisoned").team_keyboard = false;
    }

    /// Which turns the screen draws as taken back, and which of them a rewind
    /// took (those are not drawn at all). `true` when that changed, so the
    /// caller knows a frame is owed.
    pub fn mark_undone(
        &self,
        turns: std::collections::BTreeSet<u64>,
        rewound: std::collections::BTreeSet<u64>,
    ) -> bool {
        let changed = self
            .presentation
            .write()
            .expect("presentation poisoned")
            .set_undone(turns.clone(), rewound);
        if changed {
            self.moment.write().expect("moment poisoned").undone = turns;
        }
        changed
    }

    /// Deliver one committed fact to every module, with no sequence number of
    /// its own — for a caller that has only the event. A producer that needs
    /// the number (an undo naming the turn it went back to) sees zero, which
    /// matches no fact.
    pub fn absorb(&self, fact: &SessionEvent) {
        self.absorb_logged(&LoggedEvent {
            seq: 0,
            at: 0,
            event: fact.clone(),
        });
    }

    /// Let this conversation say its first word, if any producer will.
    ///
    /// **Only when the stream is empty**, and that is the whole test for "opens
    /// its top": a fresh session is empty, and a resumed one is empty too *at the
    /// moment this is asked* — `Tui::run` asks the instant the session has been
    /// described but before its history has begun to fold in (the stream is
    /// append-ordered, so the welcome has to be emitted first to sit first). A
    /// separate "is this new?" flag would be a second source of truth for one
    /// fact.
    ///
    /// **It does not go through `absorb`.** That path stands for a committed fact
    /// in the log, and what this synthesises is a way of opening, not a fact:
    /// going through it would write to `SessionLog` and be replayed on resume. So
    /// it writes the stream directly — the one place in this crate that does. A
    /// resumed conversation therefore folds a log that never had the welcome, and
    /// this re-emits it on top per view.
    ///
    /// The first producer with something to say wins and the loop stops: the rule
    /// is "when the stream is empty", and once it has spoken the stream is not.
    /// That is also why no second mechanism is needed to stop a second opener.
    ///
    /// `true` when a block was emitted, which is the caller's cue that a frame is
    /// owed.
    ///
    /// "Empty" for this purpose is "no conversational block yet": a reply a
    /// slash command left behind (the `commands` channel) is not a turn — the
    /// wizard's closing line arrives through it, and a machine that finished
    /// onboarding still owes its first word.
    pub fn open_conversation(
        &self,
        at: crate::block::Coord,
        open: &crate::module::Opening,
    ) -> bool {
        let mut stream = self.stream.write().expect("stream poisoned");
        if stream
            .slots()
            .iter()
            .any(|slot| slot.block().producer != "commands")
        {
            return false;
        }
        for producer in self.modules.producers() {
            if let Some(content) = producer.opening(at, open) {
                // Under the producer's own id: the block's `producer` field says
                // who made it, and it is what would have to settle it later.
                stream.writer(producer.id()).emit(at, content);
                return true;
            }
        }
        false
    }

    /// Deliver one committed fact to every module, as the log carries it.
    ///
    /// Producers first, then views: a view that reacts to the same fact should
    /// see a screen whose stream already contains it.
    pub fn absorb_logged(&self, logged: &LoggedEvent) {
        let fact = &logged.event;
        // When a turn opened or closed, on the clock the host was handed. Here
        // rather than in a module because this is the one place that sees both
        // the fact and the reading — and a duration on screen is the difference
        // of two readings the log does not carry (docs/adr/0008).
        match fact {
            SessionEvent::TurnStart { .. } => {
                let mut m = self.moment.write().expect("moment poisoned");
                let now = m.now;
                m.turn_started = Some(now);
                m.quiet_since = Some(now);
            }
            SessionEvent::TurnEnd { .. } => {
                let mut m = self.moment.write().expect("moment poisoned");
                m.turn_started = None;
                m.quiet_since = None;
            }
            // The newest wins, which is the whole rule the fact carries. The window
            // title takes any name; the composer pill reads `user_set` to show only
            // a name the person chose (`/rename`), not an auto first-prompt guess.
            SessionEvent::Titled {
                title, user_set, ..
            } => {
                let mut m = self.moment.write().expect("moment poisoned");
                m.title = Some(title.clone());
                m.title_user_set = *user_set;
            }
            // Anything else arriving for the turn in flight is a sign of life —
            // a chunk, a call, a result. The verb on the live row comes from the
            // fact; how long it has been since one came is this.
            _ => {
                let mut m = self.moment.write().expect("moment poisoned");
                if m.turn_started.is_some() {
                    m.quiet_since = Some(m.now);
                }
            }
        }

        // Pinned while the reader is holding a position: what the fact does to
        // the conversation is what moves the reading, and the reading has to
        // move with it or the same words slide out from under the same eyes.
        // The finished-call fold rides inside the pin too: it changes the row
        // count (the call drops from full to one row), so it must be measured
        // the same way everything else that moves the conversation is.
        self.pinned(true, || {
            self.fold(logged);
            if let SessionEvent::ToolResultLogged { call_id, .. } = fact {
                self.fold_finished_call(call_id);
            }
        });
    }

    /// Remember a tool call as auto-folded the moment its result lands — the way
    /// the reference does it: expanded while running, one row once done. Recorded
    /// in every mode (it is only *read* in the default `Full` view; the compact
    /// modes decide for themselves), so switching to the default later still
    /// shows a call that finished under another mode as one row. A hand
    /// fold/unfold still wins (see [`Presentation::fold_finished`]).
    fn fold_finished_call(&self, call_id: &str) {
        let id = {
            let stream = self.stream.read().expect("stream poisoned");
            stream.slots().iter().find_map(|s| {
                let b = s.block();
                (b.content.as_tool_call().map(|t| t.call_id.as_str()) == Some(call_id))
                    .then_some(b.id)
            })
        };
        if let Some(id) = id {
            self.presentation
                .write()
                .expect("presentation poisoned")
                .fold_finished(id);
        }
    }

    /// Fold one fact into every module, and into the few things the host keeps
    /// beside them.
    ///
    /// Split out of [`Host::absorb`] so that the pin can wrap it: the pin is
    /// "measure, change, measure again", and the change is this.
    fn fold(&self, logged: &LoggedEvent) {
        let fact = &logged.event;
        {
            let mut stream = self.stream.write().expect("stream poisoned");
            for p in self.modules.producers() {
                let mut w = stream.writer(p.id());
                p.absorb(logged, &mut w);
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
    }

    /// Tell the status line what the agent is doing.
    ///
    /// **Also pins the reading**, and that is the point of it living here
    /// rather than in the front end: `activity` is half of what the live line
    /// draws, and the other half is the turn — a fact that arrives through
    /// [`Host::absorb`]. Whichever of the two lands second is what changes the
    /// height, and which one that is is not settled anywhere. A pin in only one
    /// of the two routes would hold for half the moves, which is worse than not
    /// holding at all: the screen jumps on some turns and not others.
    ///
    /// `true` when it changed what the line would draw — the caller's business,
    /// because a frame for the same picture is a frame nobody needed.
    pub fn set_activity(&self, activity: crate::moment::Activity) -> bool {
        {
            let m = self.moment.read().expect("moment poisoned");
            // Fast path: nothing to change and no arm to spend.
            if m.activity == activity && !m.pending_working {
                return false;
            }
        }
        // Any explicit decision about what the line says supersedes a pending
        // arm — a turn that ends before its first fact (an immediate error) must
        // not leave the arm set to fire on the next turn's opening line.
        let mut visual_changed = false;
        self.pinned(true, || {
            let mut m = self.moment.write().expect("moment poisoned");
            m.pending_working = false;
            if m.activity != activity {
                m.activity = activity;
                visual_changed = true;
            }
        });
        visual_changed
    }

    /// A turn has started: remember to raise the working line, but not yet —
    /// [`Self::settle_working`] does it once the turn's first fact is folded, so
    /// the spinner never appears above the message that started the turn
    /// (the "waiting for model, then my text shows up" ordering). Never lowers a
    /// line already up, so a steering message folding into a running turn is
    /// untouched.
    ///
    /// Also spends any `已中断` note: it belongs to the turn you stopped, and a
    /// turn is running again now. `Action::Submit` clears it for a typed send,
    /// but a turn that starts without one (a scheduled or resumed prompt) arrives
    /// here instead, and the stale note must not hang under a live turn. Draws a
    /// frame only when it actually took that note down — arming alone is
    /// invisible until the arm is spent.
    pub fn arm_working(&self) -> bool {
        let mut cleared = false;
        self.pinned(true, || {
            let mut m = self.moment.write().expect("moment poisoned");
            m.pending_working = true;
            cleared = m.interrupted;
            m.interrupted = false;
        });
        cleared
    }

    /// The turn's first fact is on screen now — raise the working line if a start
    /// was waiting on it. A no-op mid-turn (nothing armed) and for a turn whose
    /// line is already up.
    pub fn settle_working(&self) -> bool {
        if !self.moment.read().expect("moment poisoned").pending_working {
            return false;
        }
        self.pinned(true, || {
            let mut m = self.moment.write().expect("moment poisoned");
            m.pending_working = false;
            m.activity = crate::moment::Activity::Working;
        });
        true
    }

    /// Run `change` with the reader's place held, unconditionally.
    ///
    /// For a change the person made by pointing at something: the row they
    /// pointed at stays where it was, whether or not they are at the bottom.
    /// See [`Host::pinned`].
    pub fn held_while<F: FnOnce()>(&self, change: F) {
        self.pinned(false, change)
    }

    /// Note words the person said that the model has not been handed yet.
    ///
    /// Called by the front end when a line is submitted during a turn. Goes
    /// through [`Host::set_steering`], so the row this adds is pinned against
    /// the reader's scroll like any other tail row — it takes a line off the
    /// conversation, and a reader studying history must not watch it slide.
    pub fn add_steering(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut next = self
            .moment
            .read()
            .expect("moment poisoned")
            .steering
            .clone();
        if !next.is_empty() {
            next.push('\n');
        }
        next.push_str(text);
        self.set_steering(next);
    }

    /// The model has been handed everything waiting, so nothing is waiting.
    ///
    /// The whole point of the panel: it is up exactly while the person has said
    /// something and the model has not seen it. `AgentEvent::Steered` is that
    /// moment — it is emitted at the round boundary the inputs were folded at,
    /// which is when the transcript starts drawing them too. Clearing at the end
    /// of the turn instead would show every steering line twice.
    pub fn clear_steering(&self) {
        self.set_steering(String::new());
    }

    /// Replace what is waiting, pinned. `false` when it did not change.
    fn set_steering(&self, text: String) -> bool {
        if self.moment.read().expect("moment poisoned").steering == text {
            return false;
        }
        self.pinned(true, || {
            self.moment.write().expect("moment poisoned").steering = text;
        });
        true
    }

    /// Whether a question is drawn as a panel riding the tail.
    ///
    /// One predicate, read by the two places that have to agree about it: where
    /// the question is drawn (the tail, or the foot of the stream) and what the
    /// composer does about it (steps aside, or stays). A second copy of this
    /// answer is a screen that hides the field for a panel that is not there.
    fn ask_panel_mounted(&self) -> bool {
        self.modules.view(crate::modules::ask::ID).is_some()
    }

    /// The light the window title carries: what is happening, or `None` when
    /// the person or the terminal has said not to show one.
    ///
    /// Here and not on [`crate::moment::Moment`] for one reason: the ask queue
    /// is this struct's, and a question is at its most urgent in the window
    /// between arriving and being drawn — which is exactly the window a light
    /// exists for. Everything else is folded off the moment, so the two cannot
    /// disagree about the turn.
    pub fn light(&self) -> Option<crate::text::Light> {
        let m = self.moment.read().expect("moment poisoned");
        if !m.status_dot_on() {
            return None;
        }
        Some(if self.asks.is_waiting() {
            crate::text::Light::Waiting
        } else {
            m.light()
        })
    }

    /// Bring `Moment::asking` in step with the queue, keeping the pointed-at row.
    ///
    /// The queue is the truth about whether a question is waiting; the moment is
    /// what a module renders from. They are two copies of one fact on purpose —
    /// a view module may not reach into the host — so this is where they are
    /// reconciled, and it is the only place that writes `asking`.
    ///
    /// The cursor survives the rewrite while the question is the same one: the
    /// loop syncs on every wake, and rebuilding the `Ask` from scratch each time
    /// would put the highlight back on the first answer under a pointer that had
    /// moved it.
    ///
    /// **True when a frame is owed.**
    pub fn sync_asking(&self) -> bool {
        // Only when the panel is mounted. Unmounted, there is no panel to draw
        // the question and no panel for the composer to step aside for: the
        // question goes to the foot of the stream the way it did before there was
        // a panel, and the composer stays exactly where it was. Gating here rather
        // than in each of the four places that read `asking` is what keeps them
        // from disagreeing about whether a question is on screen.
        let waiting = self
            .ask_panel_mounted()
            .then(|| self.asks.peek().map(|(_, q)| q))
            .flatten();
        let mut m = self.moment.write().expect("moment poisoned");
        match waiting {
            None => m.asking.take().is_some(),
            Some(question) => {
                let cursor = match &m.asking {
                    Some(had) if had.question == question => had.cursor,
                    _ => 0,
                };
                let same = m
                    .asking
                    .as_ref()
                    .is_some_and(|had| had.question == question && had.cursor == cursor);
                if same {
                    return false;
                }
                m.asking = Some(crate::moment::Ask { question, cursor });
                true
            }
        }
    }

    /// Move the highlight to an answer, by index. True when it moved.
    pub fn point_ask_at(&self, row: usize) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        m.asking.as_mut().is_some_and(|a| a.point_at(row))
    }

    /// Move the highlight by `delta` answers, clamped to the ones there are.
    ///
    /// Clamped, not wrapped: a highlight that jumps from the last answer to the
    /// first reads as a slip, and there is nowhere to fall off to at either end.
    pub fn move_ask_by(&self, delta: i32) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(ask) = m.asking.as_ref() else {
            return false;
        };
        let last = ask.question.options.len().saturating_sub(1);
        let cur = ask.cursor as i32;
        let row = (cur + delta).clamp(0, last as i32) as usize;
        m.asking.as_mut().is_some_and(|a| a.point_at(row))
    }

    /// Ask for a password on the composer's line (`crate::secret`).
    ///
    /// The queue is the truth about whether one is being asked for; the moment
    /// is what the field draws from. Two copies of one thing on purpose — a
    /// view module may not reach into the host — so every route that changes
    /// one goes through here and changes the other, which is what keeps a field
    /// showing a prompt nobody is waiting on from being possible.
    pub fn ask_secret(&self, prompt: &str, reply: tokio::sync::oneshot::Sender<Option<String>>) {
        self.secrets.ask(prompt, reply);
        self.sync_secret();
    }

    /// Whether a password is being asked for. Read by the one place focus is
    /// decided, so that "who has the keyboard" and "what the field is showing"
    /// cannot be two different answers.
    pub fn secret_waiting(&self) -> bool {
        self.secrets.is_waiting()
    }

    /// Give a key to the password prompt. `true` when it closed.
    pub fn secret_key(&self, press: crate::surface::KeyPress) -> bool {
        let closed = self.secrets.key(press);
        self.sync_secret();
        closed
    }

    /// Paste into the password rather than into the draft. `true` when the
    /// prompt closed — a pasted newline is the enter that was not pressed.
    pub fn secret_paste(&self, text: &str) -> bool {
        let closed = self.secrets.paste(text);
        self.sync_secret();
        closed
    }

    /// Refuse a password still being asked for. For shutdown, and fail-closed:
    /// the `sudo` waiting on it would otherwise hold the turn open forever.
    pub fn refuse_secret(&self) {
        self.secrets.refuse();
        self.sync_secret();
    }

    /// Bring `Moment::secret` in step with the mailbox. The only place it is
    /// written, for the reason [`Host::sync_asking`] is the only place `asking`
    /// is.
    fn sync_secret(&self) {
        let asking = self.secrets.asking();
        let mut m = self.moment.write().expect("moment poisoned");
        m.secret = asking;
    }

    /// Whether the settings panel is up.
    ///
    /// Read by everything that has to agree about it: the keys' owner, and
    /// `asked_height`, which gives the composer's rows away. One question, so
    /// that a panel drawn over a composer cannot be a panel the arbitration
    /// does not know about.
    pub fn settings_open(&self) -> bool {
        self.moment
            .read()
            .expect("moment poisoned")
            .settings_panel
            .is_some()
    }

    /// Pull the settings panel up, or put it away. True when it changed.
    ///
    /// Opening is idempotent rather than a toggle-by-accident: the panel keeps
    /// what was typed if it is already up, so a `/config` typed while it is open
    /// does not silently clear a search the person is in the middle of.
    pub fn toggle_settings(&self) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        match m.settings_panel.take() {
            Some(_) => true,
            None => {
                // The other panel goes away with it, for the reason
                // [`Host::toggle_providers`] gives: two panels a person works in
                // are two claims on one keyboard, and the one that is checked
                // second would take the composer's rows without ever seeing a
                // press. Its half-typed credential goes too.
                if m.providers_panel.take().is_some() {
                    self.providers_secret
                        .lock()
                        .expect("provider secret poisoned")
                        .clear();
                }
                m.plugins_panel = None;
                m.rewind_panel = None;
                m.settings_panel = Some(crate::settings::Panel::new());
                true
            }
        }
    }

    /// Put the settings panel away. True when it was up.
    pub fn close_settings(&self) -> bool {
        self.moment
            .write()
            .expect("moment poisoned")
            .settings_panel
            .take()
            .is_some()
    }

    /// Run one key against the settings panel.
    ///
    /// The branching lives in [`crate::settings::key`], which is pure and tested
    /// without a screen; this is the half that needs the moment — the rows the
    /// key acts on, and the panel it writes back.
    ///
    /// Returns the change to send over the seam, when the key was one that
    /// changes a setting. The caller owns the write: only it can reach the
    /// [`crate::settings::Settings`] port, and this type may not.
    /// Only [`crate::settings::Step::Set`] and
    /// [`crate::settings::Step::Reset`] ever come back here; the rest is the
    /// panel's own business and is settled above.
    pub fn settings_key(
        &self,
        press: crate::surface::KeyPress,
    ) -> (bool, Option<crate::settings::Step>) {
        let mut m = self.moment.write().expect("moment poisoned");
        let view = m.settings.clone();
        let Some(panel) = m.settings_panel.as_mut() else {
            return (false, None);
        };
        let before = panel.clone();
        let step = crate::settings::key(&view, panel, press);
        let changed = *panel != before;
        match step {
            // The edit closes here rather than in the caller, so the panel that
            // is drawn is never one still holding a value that has already been
            // sent.
            step @ (crate::settings::Step::Set { .. } | crate::settings::Step::Reset { .. }) => {
                (true, Some(step))
            }
            crate::settings::Step::Close => {
                m.settings_panel = None;
                (true, None)
            }
            crate::settings::Step::Stay => (changed, None),
        }
    }

    /// Which answer a screen row belongs to, when it belongs to one.
    ///
    /// Read off the rect the panel was **drawn** in, so a click and the drawn
    /// highlight cannot disagree. The row is converted to an answer index here
    /// rather than by the caller for the same reason `menu::Menu::click` computes
    /// it from `Menu::rect`: two formulas for one layout is a panel whose rows
    /// answer to the wrong one.
    ///
    /// `None` means the point is not on an answer — beside the panel, on its
    /// prompt, above the first row — and the caller decides what that means.
    pub fn answer_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let rect = *self.hits.lock().expect("hits poisoned").ask.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        let ask = m.asking.as_ref()?;
        let vp = crate::moment::Viewport::new(rect, &m);
        let geom = crate::modules::ask::geometry(&ask.question, &vp);
        geom.answer_at((y - rect.y) as usize)
    }

    /// Which page tab is under this cell, when one is.
    ///
    /// Read off the rect the panel was **drawn** in, like every other hit test
    /// here, and answered only on the panel's **first row**: the tabs live there
    /// and nowhere else, so a press three rows down must not switch pages
    /// because the cell happens to line up with a tab's column.
    ///
    /// `None` when the panel is down, when the point is not on it, when it is
    /// not the header row, or when the cell is on the title or the gaps between
    /// tabs — those are not tabs, and a press on them is a press on the panel.
    pub fn settings_tab_at(&self, x: u16, y: u16) -> Option<crate::settings::Tab> {
        let rect = *self.hits.lock().expect("hits poisoned").settings.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        if y != rect.y {
            return None;
        }
        crate::modules::settings::tab_at((x - rect.x) as usize)
    }

    /// Which inner page is under the pointer, when the pointer is on the row
    /// that carries them.
    ///
    /// Read off the same layout the frame drew with, the way every other hit
    /// test on this panel is: the inner row moves with the page's chrome, and a
    /// hit test that assumed a fixed screen row would answer for whatever
    /// happened to be there instead.
    pub fn settings_stats_page_at(&self, x: u16, y: u16) -> Option<crate::settings::StatsPage> {
        let rect = *self.hits.lock().expect("hits poisoned").settings.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        let vp = crate::moment::Viewport::new(rect, &m);
        let row = crate::modules::settings::geometry(&m, &vp).pages_row()?;
        if (y - rect.y) as usize != row {
            return None;
        }
        crate::modules::settings::stats_page_at((x - rect.x) as usize)
    }

    /// Show one of a page's inner pages. True when it changed.
    pub fn show_stats_page(&self, page: crate::settings::StatsPage) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.settings_panel.as_mut() else {
            return false;
        };
        if panel.stats == page {
            return false;
        }
        panel.stats = page;
        // A different page, read from its top — the same rule the arrows follow.
        panel.scroll = 0;
        true
    }

    /// Show a page. True when it changed.
    ///
    /// The same `Panel::show` the keyboard reaches, so a click and a tab press
    /// cannot come to mean different things — including the part where a page
    /// switch gives up a field with the keyboard, which is why this goes through
    /// the panel rather than setting the tab itself.
    pub fn show_settings_tab(&self, tab: crate::settings::Tab) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        match m.settings_panel.as_mut() {
            Some(panel) => panel.show(tab),
            None => false,
        }
    }

    /// Point the settings panel at a row, by index. True when it moved.
    ///
    /// Clamped to the rows the *filtered* list has, because that is what
    /// `settings_row_at` returns an index into: a pointer on the last row of a
    /// search that matched two settings means the second of those two, not the
    /// second of the catalog.
    pub fn point_settings_at(&self, row: usize) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        // Both reads first, because the borrow of `settings_panel` has to end
        // before it can be borrowed mutably: with no panel up, or no row to
        // point at, there is nothing to move and the answer is `false`.
        let rows = match m.settings_panel.as_ref() {
            Some(panel) => m.settings.matching(&panel.query).len(),
            None => return false,
        };
        match m.settings_panel.as_mut() {
            Some(panel) => panel.point_at(row, rows),
            None => false,
        }
    }

    /// Which setting a screen row belongs to, when it belongs to one.
    ///
    /// The same rule as [`Host::answer_row_at`], and for the same reason: read
    /// off the rect the panel was **drawn** in, so a click and the drawn
    /// highlight cannot come from two different arrangements of one list. The
    /// panel floats at the tail, so where it sits is a consequence of how tall
    /// everything below it turned out — re-deriving that at the press would be a
    /// second layout to keep in step with the frame.
    ///
    /// `None` when the panel is not up, when the point is not on it, or when it
    /// is on a row that is not a setting — the search box, the blank above the
    /// list, the legend. An edit in progress answers `None` too: the keyboard is
    /// already in that field, and a press would only move the highlight out from
    /// under the person's hands.
    ///
    /// "The panel is not up" is not checked here, and that is deliberate rather
    /// than an omission: [`crate::modules::settings::geometry`] lays out no rows
    /// at all without a panel to lay out, so it already answers `None` for a
    /// rect left over from an older frame — and a second check here would be the
    /// same fact stated twice, which is how the two come to disagree. The
    /// criterion that pins it is `a_click_missing_the_panel_hits_nothing`.
    /// Scroll the settings panel under the pointer, and say whether it took it.
    ///
    /// Only the pages that are read rather than filtered scroll, and only when
    /// the pointer is actually over the panel: a wheel anywhere else belongs to
    /// the conversation, which is the thing people scroll all day. Answering
    /// every wheel event while the panel happened to be open would take the
    /// wheel away from the transcript the panel does not even cover.
    pub fn settings_wheel(&self, x: u16, y: u16, by: i32) -> bool {
        let over = self
            .hits
            .lock()
            .expect("hits poisoned")
            .settings
            .is_some_and(|rect| rect.contains(x, y));
        if !over {
            return false;
        }
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.settings_panel.as_mut() else {
            return false;
        };
        if panel.tab == crate::settings::Tab::Config {
            return false;
        }
        // Clamped at the top here and at the bottom where the rows are counted:
        // this side does not know how long the page is.
        panel.scroll = match by < 0 {
            true => panel.scroll.saturating_sub(by.unsigned_abs() as usize),
            false => panel.scroll.saturating_add(by as usize),
        };
        true
    }

    pub fn settings_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let rect = *self.hits.lock().expect("hits poisoned").settings.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        let vp = crate::moment::Viewport::new(rect, &m);
        let geom = crate::modules::settings::geometry(&m, &vp);
        geom.setting_at((y - rect.y) as usize)
    }

    /// Whether the providers panel is up.
    pub fn providers_open(&self) -> bool {
        self.moment
            .read()
            .expect("moment poisoned")
            .providers_panel
            .is_some()
    }

    /// Pull the providers panel up, or put it away. True when it changed.
    ///
    /// Opening it puts the settings panel away. They are the same kind of thing
    /// — a panel a person works their configuration in — and two of them up at
    /// once would be two claims on one keyboard: the key routing decides focus
    /// in one place (`docs/adr/0022`), and a second panel under the first would
    /// take its rows without ever seeing a press.
    ///
    /// Opening is idempotent rather than a toggle-by-accident, the same bargain
    /// [`Host::toggle_settings`] strikes: a `/provider` typed while it is open
    /// keeps what was typed into it.
    pub fn toggle_providers(&self) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        match m.providers_panel.take() {
            Some(_) => {
                self.providers_secret
                    .lock()
                    .expect("provider secret poisoned")
                    .clear();
                true
            }
            None => {
                // Nothing to draw it with is a refusal, not an empty panel: a
                // panel that is "up" while its module is unmounted would take
                // the composer's rows and every key and show neither.
                if !self.modules.has_view(crate::modules::providers::ID) {
                    return false;
                }
                m.settings_panel = None;
                m.plugins_panel = None;
                m.tools_panel = None;
                m.rewind_panel = None;
                m.providers_panel = Some(crate::providers::Panel::new());
                true
            }
        }
    }

    /// Open the providers panel on its 模型 list, opening it if it is not
    /// already up. What `/model` lands on.
    ///
    /// Opening-if-closed rather than a toggle: `/model` means "show me the
    /// models", never "hide them if they happen to be up". Returns false only
    /// when there is nothing to draw the panel with — the same refusal
    /// [`Self::toggle_providers`] gives — so the caller can say so.
    pub fn open_providers_on_models(&self) -> bool {
        if !self.providers_open() && !self.toggle_providers() {
            return false;
        }
        self.show_providers_tab(crate::providers::Tab::Models);
        true
    }

    /// Put the providers panel away. True when it was up.
    ///
    /// Forgets the key that was being typed into it, wherever the close came
    /// from: a credential that outlived the form it was typed into would be sent
    /// with whatever was saved next.
    pub fn close_providers(&self) -> bool {
        let gone = self
            .moment
            .write()
            .expect("moment poisoned")
            .providers_panel
            .take()
            .is_some();
        if gone {
            self.providers_secret
                .lock()
                .expect("provider secret poisoned")
                .clear();
        }
        gone
    }

    /// Put what the launcher read into the moment. True when it changed.
    pub fn show_providers(&self, view: crate::providers::ProvidersView) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        if m.providers == view {
            return false;
        }
        m.providers = view;
        true
    }

    /// Run one key against the providers panel: the panel it writes back, and
    /// the write to send over the seam when the key asked for one.
    ///
    /// The key being typed is lent for the press and never leaves this method
    /// except inside the [`Step`](crate::providers::Step) that carries it to the
    /// port — which is the whole reason the string lives on the host rather than
    /// on the form.
    pub fn providers_key(
        &self,
        press: crate::surface::KeyPress,
    ) -> (bool, Option<crate::providers::Step>) {
        let mut m = self.moment.write().expect("moment poisoned");
        let view = m.providers.clone();
        let mut secret = self
            .providers_secret
            .lock()
            .expect("provider secret poisoned");
        let Some(panel) = m.providers_panel.as_mut() else {
            return (false, None);
        };
        let before = panel.clone();
        let step = crate::providers::key(&view, panel, &mut secret, press);
        let changed = *panel != before;
        match step {
            crate::providers::Step::Stay => (changed, None),
            crate::providers::Step::Close => {
                m.providers_panel = None;
                secret.clear();
                (true, None)
            }
            // A write, a switch or a delete: the panel has already put its form
            // away, so what is drawn next is never a form holding something that
            // has been sent.
            step => (true, Some(step)),
        }
    }

    /// The wheel over the providers panel walks its list.
    ///
    /// The cursor *is* the scroll here: the window is drawn around it
    /// (`crate::modules::providers::window`), so there is no second offset that
    /// could disagree with where the highlight is — which is the bug a panel
    /// with both a cursor and a scroll always eventually has.
    pub fn providers_wheel(&self, x: u16, y: u16, by: i32) -> bool {
        let over = self
            .hits
            .lock()
            .expect("hits poisoned")
            .providers
            .is_some_and(|rect| rect.contains(x, y));
        if !over {
            return false;
        }
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.providers_panel.as_ref() {
            // A form has no list to walk, and a wheel over it must not scroll
            // the conversation behind it either — it is still the panel's.
            Some(panel) if panel.form.is_some() => return true,
            Some(panel) => m.providers.listed(panel).len(),
            None => return false,
        };
        let Some(panel) = m.providers_panel.as_mut() else {
            return false;
        };
        let want = match by < 0 {
            true => panel.cursor.saturating_sub(by.unsigned_abs() as usize),
            false => panel.cursor.saturating_add(by as usize),
        };
        panel.point_at(want, rows);
        true
    }

    /// Show one account's models, as walking into it from the account list does.
    ///
    /// Used after an account is added: an account with no model under it cannot
    /// be talked to, so the panel answers "what now" by standing where the one
    /// thing left to do is.
    pub fn walk_into_provider(&self, account: &str) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let view = m.providers.clone();
        let Some(panel) = m.providers_panel.as_mut() else {
            return false;
        };
        panel.show(crate::providers::Tab::Models);
        panel.drill = Some(account.to_string());
        view.settle_cursor(panel);
        true
    }

    /// Put a paste into whatever the providers panel has the keyboard on.
    ///
    /// The composer is not on screen while the panel is up, so a paste that fell
    /// through to it would be text typed into a field nobody can see — which is
    /// where it went until this existed.
    pub fn providers_paste(&self, text: &str) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let mut secret = self
            .providers_secret
            .lock()
            .expect("provider secret poisoned");
        let Some(panel) = m.providers_panel.as_mut() else {
            return false;
        };
        crate::providers::paste(panel, &mut secret, text)
    }

    /// Show a list, by index. True when it moved.
    pub fn show_providers_tab(&self, tab: crate::providers::Tab) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        // The view first: the models list opens on an account's heading, which
        // is not a row the cursor may rest on.
        let view = m.providers.clone();
        match m.providers_panel.as_mut() {
            Some(panel) => {
                let moved = panel.show(tab);
                view.settle_cursor(panel);
                moved
            }
            None => false,
        }
    }

    /// Which list is under the pointer, when it is on the header row.
    pub fn providers_tab_at(&self, x: u16, y: u16) -> Option<crate::providers::Tab> {
        let rect = *self
            .hits
            .lock()
            .expect("hits poisoned")
            .providers
            .as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        // Which row the tabs are on is a fact about the layout that was drawn.
        // This panel opens with a rule, so they are **not** on its first row —
        // the assumption that they were is why a click on a tab did nothing at
        // all until the first real run of the panel found it.
        let m = self.moment.read().expect("moment poisoned");
        let vp = crate::moment::Viewport::new(rect, &m);
        let header = crate::modules::providers::geometry(&m, &vp).header_row()?;
        if (y - rect.y) as usize != header {
            return None;
        }
        crate::modules::providers::tab_at((x - rect.x) as usize)
    }

    /// Which listed row is under the pointer, when it is on one.
    ///
    /// Read off the rect the panel was **drawn** in, for the reason
    /// [`Host::settings_row_at`] is: the tail's split decides where the panel
    /// sits, and a formula that re-derived it would be a second layout to keep
    /// in step with the one on screen.
    pub fn providers_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let rect = *self
            .hits
            .lock()
            .expect("hits poisoned")
            .providers
            .as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        let vp = crate::moment::Viewport::new(rect, &m);
        crate::modules::providers::geometry(&m, &vp).listed_at((y - rect.y) as usize)
    }

    /// Point the panel at a listed row. True when it moved.
    pub fn point_providers_at(&self, row: usize) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.providers_panel.as_ref() {
            Some(panel) => m.providers.listed(panel).len(),
            None => return false,
        };
        match m.providers_panel.as_mut() {
            Some(panel) => panel.point_at(row, rows),
            None => false,
        }
    }

    // ---- the plugins panel -------------------------------------------------
    //
    // The same dozen methods the providers panel has, and deliberately the same
    // shape: three panels a person works in, one way of opening them, one way of
    // routing a key into them, one way of finding the row a click landed on. A
    // third panel that invented its own would be a third place to fix the next
    // thing any of them gets wrong.

    /// Whether the plugins panel is up.
    pub fn plugins_open(&self) -> bool {
        self.moment
            .read()
            .expect("moment poisoned")
            .plugins_panel
            .is_some()
    }

    /// Pull the plugins panel up, or put it away. True when it changed.
    ///
    /// Opening it puts the other two away, for the reason
    /// [`Host::toggle_providers`] gives, and is idempotent: a `/plugin` typed
    /// while it is open keeps what was typed into it.
    pub fn toggle_plugins(&self) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        match m.plugins_panel.take() {
            Some(_) => true,
            None => {
                // Nothing to draw it with is a refusal, not an empty panel — the
                // same bargain `toggle_providers` strikes.
                if !self.modules.has_view(crate::modules::plugins::ID) {
                    return false;
                }
                m.settings_panel = None;
                if m.providers_panel.take().is_some() {
                    self.providers_secret
                        .lock()
                        .expect("provider secret poisoned")
                        .clear();
                }
                m.tools_panel = None;
                m.rewind_panel = None;
                m.plugins_panel = Some(crate::plugins::Panel::new());
                true
            }
        }
    }

    /// Put the plugins panel away. True when it was up.
    pub fn close_plugins(&self) -> bool {
        self.moment
            .write()
            .expect("moment poisoned")
            .plugins_panel
            .take()
            .is_some()
    }

    /// Put what the launcher read into the moment. True when it changed.
    pub fn show_plugins(&self, view: crate::plugins::PluginsView) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        if m.plugins == view {
            return false;
        }
        m.plugins = view;
        true
    }

    /// Say that a job is running, or that it is over.
    ///
    /// Set before the work is sent out and cleared when it lands, so the panel
    /// has something true to draw for the seconds a `git clone` takes — and so
    /// the keys that would start a second one are swallowed while the first is
    /// still going (`crate::plugins::key`).
    pub fn plugins_busy(&self, busy: Option<crate::plugins::Busy>) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.plugins_panel.as_mut() else {
            return false;
        };
        if panel.busy == busy {
            return false;
        }
        panel.busy = busy;
        true
    }

    /// Run one key against the plugins panel: the panel it writes back, and the
    /// work to send over the seam when the key asked for some.
    pub fn plugins_key(
        &self,
        press: crate::surface::KeyPress,
    ) -> (bool, Option<crate::plugins::Step>) {
        let mut m = self.moment.write().expect("moment poisoned");
        let view = m.plugins.clone();
        let Some(panel) = m.plugins_panel.as_mut() else {
            return (false, None);
        };
        let before = panel.clone();
        let step = crate::plugins::key(&view, panel, press);
        let changed = *panel != before;
        match step {
            crate::plugins::Step::Stay => (changed, None),
            crate::plugins::Step::Close => {
                m.plugins_panel = None;
                (true, None)
            }
            step => (true, Some(step)),
        }
    }

    /// The wheel over the plugins panel walks its list.
    pub fn plugins_wheel(&self, x: u16, y: u16, by: i32) -> bool {
        let over = self
            .hits
            .lock()
            .expect("hits poisoned")
            .plugins
            .is_some_and(|rect| rect.contains(x, y));
        if !over {
            return false;
        }
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.plugins_panel.as_ref() {
            // A form or a running job has no list to walk, and the wheel is
            // still the panel's — it must not scroll the conversation behind it.
            Some(panel) if panel.form.is_some() || panel.busy.is_some() => return true,
            Some(panel) => m.plugins.listed(panel).len(),
            None => return false,
        };
        let Some(panel) = m.plugins_panel.as_mut() else {
            return false;
        };
        let want = match by < 0 {
            true => panel.cursor.saturating_sub(by.unsigned_abs() as usize),
            false => panel.cursor.saturating_add(by as usize),
        };
        panel.point_at(want, rows);
        true
    }

    /// Put a paste into whatever the plugins panel has the keyboard on.
    ///
    /// The composer is not on screen while the panel is up, so a paste that fell
    /// through to it would be text typed into a field nobody can see — and a
    /// marketplace URL is exactly the thing people paste.
    pub fn plugins_paste(&self, text: &str) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.plugins_panel.as_mut() else {
            return false;
        };
        crate::plugins::paste(panel, text)
    }

    /// Show a page, by index. True when it moved.
    pub fn show_plugins_tab(&self, tab: crate::plugins::Tab) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        match m.plugins_panel.as_mut() {
            Some(panel) => panel.show(tab),
            None => false,
        }
    }

    /// Which page is under the pointer, when it is on the header row.
    pub fn plugins_tab_at(&self, x: u16, y: u16) -> Option<crate::plugins::Tab> {
        let rect = *self.hits.lock().expect("hits poisoned").plugins.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        let vp = crate::moment::Viewport::new(rect, &m);
        let header = crate::modules::plugins::geometry(&m, &vp).header_row()?;
        if (y - rect.y) as usize != header {
            return None;
        }
        crate::modules::plugins::tab_at(&m, (x - rect.x) as usize)
    }

    /// Which listed row is under the pointer.
    pub fn plugins_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let rect = *self.hits.lock().expect("hits poisoned").plugins.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        let vp = crate::moment::Viewport::new(rect, &m);
        crate::modules::plugins::geometry(&m, &vp).listed_at((y - rect.y) as usize)
    }

    /// Point the panel at a listed row. True when it moved.
    pub fn point_plugins_at(&self, row: usize) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.plugins_panel.as_ref() {
            Some(panel) => m.plugins.listed(panel).len(),
            None => return false,
        };
        match m.plugins_panel.as_mut() {
            Some(panel) => panel.point_at(row, rows),
            None => false,
        }
    }

    // ---- the tools panel ---------------------------------------------------
    //
    // The fourth panel, and deliberately the same dozen methods: one way of
    // opening, one way of routing a key, one way of finding the row a click
    // landed on. A fourth panel that invented its own would be a fourth place
    // to fix the next thing any of them gets wrong.

    /// Whether the tools panel is up.
    pub fn tools_open(&self) -> bool {
        self.moment
            .read()
            .expect("moment poisoned")
            .tools_panel
            .is_some()
    }

    /// Pull the tools panel up, or put it away. True when it changed.
    pub fn toggle_tools(&self) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        match m.tools_panel.take() {
            Some(_) => true,
            None => {
                if !self.modules.has_view(crate::modules::tools::ID) {
                    return false;
                }
                m.settings_panel = None;
                m.plugins_panel = None;
                if m.providers_panel.take().is_some() {
                    self.providers_secret
                        .lock()
                        .expect("provider secret poisoned")
                        .clear();
                }
                m.rewind_panel = None;
                m.tools_panel = Some(crate::tools::Panel::new());
                true
            }
        }
    }

    /// Put the tools panel away. True when it was up.
    pub fn close_tools(&self) -> bool {
        self.moment
            .write()
            .expect("moment poisoned")
            .tools_panel
            .take()
            .is_some()
    }

    /// Put what the host answered into the moment. True when it changed.
    pub fn show_tools(&self, view: crate::tools::ToolsView) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        if m.tools == view {
            return false;
        }
        m.tools = view;
        true
    }

    /// Say that a switch is on its way there and back, or that it landed.
    pub fn tools_busy(&self, busy: Option<crate::tools::Busy>) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.tools_panel.as_mut() else {
            return false;
        };
        if panel.busy == busy {
            return false;
        }
        panel.busy = busy;
        true
    }

    /// Say what the last key came to, when it came to something worth reading.
    pub fn tools_note(&self, note: Option<String>) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.tools_panel.as_mut() else {
            return false;
        };
        if panel.note == note {
            return false;
        }
        panel.note = note;
        true
    }

    /// Run one key against the tools panel: the panel it writes back, and the
    /// work to send over the seam when the key asked for some.
    pub fn tools_key(&self, press: crate::surface::KeyPress) -> (bool, Option<crate::tools::Step>) {
        let mut m = self.moment.write().expect("moment poisoned");
        let view = m.tools.clone();
        let Some(panel) = m.tools_panel.as_mut() else {
            return (false, None);
        };
        let before = panel.clone();
        let step = crate::tools::key(&view, panel, press);
        let changed = *panel != before;
        match step {
            crate::tools::Step::Stay => (changed, None),
            crate::tools::Step::Close => {
                m.tools_panel = None;
                (true, None)
            }
            step => (true, Some(step)),
        }
    }

    /// The wheel over the tools panel walks its list.
    pub fn tools_wheel(&self, x: u16, y: u16, by: i32) -> bool {
        let over = self
            .hits
            .lock()
            .expect("hits poisoned")
            .tools
            .is_some_and(|rect| rect.contains(x, y));
        if !over {
            return false;
        }
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.tools_panel.as_ref() {
            // A switch in flight has no list to walk, and the wheel is still the
            // panel's — it must not scroll the conversation behind it.
            Some(panel) if panel.busy.is_some() => return true,
            Some(panel) => m.tools.listed(panel).len(),
            None => return false,
        };
        let Some(panel) = m.tools_panel.as_mut() else {
            return false;
        };
        let want = match by < 0 {
            true => panel.cursor.saturating_sub(by.unsigned_abs() as usize),
            false => panel.cursor.saturating_add(by as usize),
        };
        panel.point_at(want, rows);
        true
    }

    /// Put a paste into the tools panel's search box: a tool name is exactly
    /// the thing that arrives by paste, and the composer is not on screen.
    pub fn tools_paste(&self, text: &str) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.tools_panel.as_mut() else {
            return false;
        };
        crate::tools::paste(panel, text)
    }

    /// Which listed row is under the pointer.
    pub fn tools_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let rect = *self.hits.lock().expect("hits poisoned").tools.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        let vp = crate::moment::Viewport::new(rect, &m);
        crate::modules::tools::geometry(&m, &vp).row_at((y - rect.y) as usize)
    }

    /// Point the panel at a listed row. True when it moved.
    pub fn point_tools_at(&self, row: usize) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.tools_panel.as_ref() {
            Some(panel) => m.tools.listed(panel).len(),
            None => return false,
        };
        match m.tools_panel.as_mut() {
            Some(panel) => panel.point_at(row, rows),
            None => false,
        }
    }

    // ---- the rewind panel ---------------------------------------------------
    //
    // The fifth panel, and deliberately the same dozen methods as the other
    // four. What is different is only what it is *for*: the other four change
    // what this build is, and this one takes back what it did.

    /// Whether the rewind panel is up.
    pub fn rewind_open(&self) -> bool {
        self.moment
            .read()
            .expect("moment poisoned")
            .rewind_panel
            .is_some()
    }

    /// Pull the rewind panel up, or put it away. True when it changed.
    pub fn toggle_rewind(&self) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        match m.rewind_panel.take() {
            Some(_) => true,
            None => {
                // Nothing to draw it with is a refusal, not an empty panel — the
                // same bargain the other four strike.
                if !self.modules.has_view(crate::modules::rewind::ID) {
                    return false;
                }
                m.settings_panel = None;
                m.plugins_panel = None;
                m.tools_panel = None;
                if m.providers_panel.take().is_some() {
                    self.providers_secret
                        .lock()
                        .expect("provider secret poisoned")
                        .clear();
                }
                m.rewind_panel = Some(crate::rewind::Panel::new());
                true
            }
        }
    }

    /// Put the rewind panel away. True when it was up.
    pub fn close_rewind(&self) -> bool {
        self.moment
            .write()
            .expect("moment poisoned")
            .rewind_panel
            .take()
            .is_some()
    }

    /// Put what the host answered into the moment, and rest the cursor on
    /// "(current)". True when it changed.
    ///
    /// **The cursor goes back to the bottom on every answer**, including the one
    /// that follows a rewind: the list it was pointing into is not the list that
    /// came back, and a cursor left at row 3 of a shorter list is a panel aimed
    /// at whatever slid under it.
    pub fn show_rewind(&self, view: crate::rewind::RewindView) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = view.rows();
        let mut changed = false;
        if m.rewind != view {
            m.rewind = view;
            changed = true;
        }
        if let Some(panel) = m.rewind_panel.as_mut() {
            let was = panel.cursor;
            panel.rest_at_current(rows);
            changed |= panel.cursor != was;
        }
        changed
    }

    /// Say that a rewind is on its way there and back, or that it landed.
    pub fn rewind_busy(&self, busy: Option<crate::rewind::Busy>) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.rewind_panel.as_mut() else {
            return false;
        };
        if panel.busy == busy {
            return false;
        }
        panel.busy = busy;
        true
    }

    /// Say what the last key came to, when it came to something worth reading.
    pub fn rewind_note(&self, note: Option<String>) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.rewind_panel.as_mut() else {
            return false;
        };
        if panel.note == note {
            return false;
        }
        panel.note = note;
        true
    }

    /// Run one key against the rewind panel: the panel it writes back, and the
    /// work to send over the seam when the key asked for some.
    pub fn rewind_key(
        &self,
        press: crate::surface::KeyPress,
    ) -> (bool, Option<crate::rewind::Step>) {
        let mut m = self.moment.write().expect("moment poisoned");
        let view = m.rewind.clone();
        let Some(panel) = m.rewind_panel.as_mut() else {
            return (false, None);
        };
        let before = panel.clone();
        let step = crate::rewind::key(&view, panel, press);
        let changed = *panel != before;
        match step {
            crate::rewind::Step::Stay => (changed, None),
            crate::rewind::Step::Close => {
                m.rewind_panel = None;
                (true, None)
            }
            step => (true, Some(step)),
        }
    }

    /// The wheel over the rewind panel walks its list.
    pub fn rewind_wheel(&self, x: u16, y: u16, by: i32) -> bool {
        let over = self
            .hits
            .lock()
            .expect("hits poisoned")
            .rewind
            .is_some_and(|rect| rect.contains(x, y));
        if !over {
            return false;
        }
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.rewind_panel.as_ref() {
            // A rewind in flight has no list to walk, and the wheel is still the
            // panel's — it must not scroll the conversation behind it.
            Some(panel) if panel.busy.is_some() => return true,
            Some(_) => m.rewind.rows(),
            None => return false,
        };
        let Some(panel) = m.rewind_panel.as_mut() else {
            return false;
        };
        let want = match by < 0 {
            true => panel.cursor.saturating_sub(by.unsigned_abs() as usize),
            false => panel.cursor.saturating_add(by as usize),
        };
        panel.point_at(want, rows);
        true
    }

    /// Which row of the list is under the pointer — a turn, or "(current)".
    pub fn rewind_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let rect = *self.hits.lock().expect("hits poisoned").rewind.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        let vp = crate::moment::Viewport::new(rect, &m);
        crate::modules::rewind::geometry(&m, &vp).row_at((y - rect.y) as usize)
    }

    /// Point the panel at a row. True when it moved.
    pub fn point_rewind_at(&self, row: usize) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.rewind_panel.as_ref() {
            Some(_) => m.rewind.rows(),
            None => return false,
        };
        match m.rewind_panel.as_mut() {
            Some(panel) => panel.point_at(row, rows),
            None => false,
        }
    }

    // ---- the resume panel -----------------------------------------------------

    /// Whether the resume panel is up.
    pub fn resume_open(&self) -> bool {
        self.moment
            .read()
            .expect("moment poisoned")
            .resume_panel
            .is_some()
    }

    /// Bring the resume panel up (idempotent), putting away any other panel a
    /// hand works in. `/resume` means "show me the sessions", never "hide them
    /// if they happen to be up" — so it opens rather than toggles. False only
    /// when there is nothing to draw it with (the module was not mounted).
    pub fn open_resume(&self) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        if m.resume_panel.is_some() {
            return true;
        }
        if !self.modules.has_view(crate::modules::resume::ID) {
            return false;
        }
        m.settings_panel = None;
        if m.providers_panel.take().is_some() {
            self.providers_secret
                .lock()
                .expect("provider secret poisoned")
                .clear();
        }
        m.plugins_panel = None;
        m.tools_panel = None;
        m.rewind_panel = None;
        m.resume_panel = Some(crate::resume::Panel::new());
        true
    }

    /// Put the resume panel away. True when it was up.
    pub fn close_resume(&self) -> bool {
        self.moment
            .write()
            .expect("moment poisoned")
            .resume_panel
            .take()
            .is_some()
    }

    /// Put the sessions the host answered into the moment, and rest the cursor at
    /// the top: the list it was pointing into is not the list that came back.
    pub fn show_resume(&self, view: crate::resume::ResumeView) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let mut changed = false;
        if m.resume != view {
            m.resume = view;
            changed = true;
        }
        if let Some(panel) = m.resume_panel.as_mut() {
            if panel.cursor != 0 {
                panel.cursor = 0;
                changed = true;
            }
        }
        changed
    }

    /// Take a session out of the list the panel is showing.
    ///
    /// Called when the host said it is gone. The screen does not ask for the
    /// list again: it was just told what changed, and a second answer to the
    /// same question is another chance for the two to disagree. The cursor
    /// stays where it is (clamped by the drawing), so the next Delete is aimed
    /// at the row that moved up — which is why arming is cleared here too.
    pub fn forget_resume(&self, id: &str) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let left: Vec<crate::resume::Session> = m
            .resume
            .sessions()
            .iter()
            .filter(|session| session.id != id)
            .cloned()
            .collect();
        if left.len() == m.resume.sessions().len() {
            return false;
        }
        m.resume = crate::resume::ResumeView::new(left);
        if let Some(panel) = m.resume_panel.as_mut() {
            panel.armed = None;
        }
        true
    }

    /// Run one key against the resume panel: the panel it writes back, and the
    /// session to resume when the key asked for one.
    pub fn resume_key(
        &self,
        press: crate::surface::KeyPress,
    ) -> (bool, Option<crate::resume::Step>) {
        let mut m = self.moment.write().expect("moment poisoned");
        let view = m.resume.clone();
        let Some(panel) = m.resume_panel.as_mut() else {
            return (false, None);
        };
        let before = panel.clone();
        let step = crate::resume::key(&view, panel, press);
        let changed = *panel != before;
        match step {
            crate::resume::Step::Stay => (changed, None),
            crate::resume::Step::Close => {
                m.resume_panel = None;
                (true, None)
            }
            step => (true, Some(step)),
        }
    }

    /// The wheel over the resume panel walks its list.
    pub fn resume_wheel(&self, x: u16, y: u16, by: i32) -> bool {
        let over = self
            .hits
            .lock()
            .expect("hits poisoned")
            .resume
            .is_some_and(|rect| rect.contains(x, y));
        if !over {
            return false;
        }
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.resume_panel.as_ref() {
            Some(panel) => m.resume.rows(panel),
            None => return false,
        };
        let Some(panel) = m.resume_panel.as_mut() else {
            return false;
        };
        let want = match by < 0 {
            true => panel.cursor.saturating_sub(by.unsigned_abs() as usize),
            false => panel.cursor.saturating_add(by as usize),
        };
        panel.point_at(want, rows);
        true
    }

    /// Which row of the list is under the pointer.
    pub fn resume_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let rect = *self.hits.lock().expect("hits poisoned").resume.as_ref()?;
        if !rect.contains(x, y) {
            return None;
        }
        let m = self.moment.read().expect("moment poisoned");
        let vp = crate::moment::Viewport::new(rect, &m);
        crate::modules::resume::geometry(&m, &vp).listed_at((y - rect.y) as usize)
    }

    /// Point the panel at a row. True when it moved.
    pub fn point_resume_at(&self, row: usize) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let rows = match m.resume_panel.as_ref() {
            Some(panel) => m.resume.rows(panel),
            None => return false,
        };
        match m.resume_panel.as_mut() {
            Some(panel) => panel.point_at(row, rows),
            None => false,
        }
    }

    /// Run `change`, keeping the reader's place across whatever it did.
    ///
    /// **Measure, change, measure again** — one shape, because the arithmetic is
    /// the same on every route and there are three of them: a fact folded in
    /// ([`Host::absorb`]), the activity the event loop writes
    /// ([`Host::set_activity`]), and a click that folds a block. Three copies
    /// would drift, and the drift is invisible until someone is reading history
    /// while it happens.
    ///
    /// What it holds still is the *reading*: the offset is measured from the
    /// bottom of the conversation and the newest content lands at that bottom,
    /// so a reader who scrolled up to study something would watch it slide away
    /// by exactly as much as arrived. Moving the offset by what grew keeps the
    /// same lines under the same eyes.
    ///
    /// `only_when_held`: true for the routes where "at the bottom" means follow
    /// the output (the whole point there is to keep up, and the walk is
    /// O(conversation)). **False for a click**, which is different: the person
    /// pointed at a row, and the row has to stay where they pointed. A block
    /// that unfolds grows *upward* — its header is the first thing to leave —
    /// and that is exactly the most common case, done at the bottom.
    ///
    /// The gate is what makes the routes add up. A turn ending parts a fact on
    /// the emitter's thread and an activity on the event loop's, and if both
    /// measure the same change and both compensate for it, the offset moves
    /// twice for one screenful.
    fn pinned(&self, only_when_held: bool, change: impl FnOnce()) {
        let _gate = self.pin_gate.lock().expect("pin gate poisoned");
        let room = *self.last_room.lock().expect("room poisoned");
        if room.is_empty() {
            // No frame has been composed yet, so there is no "where the reader
            // is" to hold and nothing to measure against.
            change();
            return;
        }
        let before = self.moment.read().expect("moment poisoned").clone();
        let held = !only_when_held || before.scroll.0 > 0;
        let before_h = if held {
            self.stream_height_in(room, &before).0
        } else {
            0
        };

        change();

        if !held {
            return;
        }
        // Measured again *after* the change, and against the moment as it is
        // now — a fresh read, not the snapshot. The snapshot is what the
        // *before* height was taken at, and reusing it here would measure the
        // same world twice and find no difference at all: this is exactly the
        // hole the click route fell through when the fold lived somewhere the
        // snapshot could not see it.
        let after = self.moment.read().expect("moment poisoned").clone();
        let after_h = self.stream_height_in(room, &after).0;
        let grew = after_h as i64 - before_h as i64;
        if grew == 0 {
            return;
        }
        let mut m = self.moment.write().expect("moment poisoned");
        let max = self.scroll_limit((room.w, room.h), &after) as i64;
        m.scroll = crate::moment::ScrollPos((m.scroll.0 as i64 + grew).clamp(0, max) as usize);
    }

    pub fn painted(&self) -> u64 {
        *self.painted.lock().expect("counter poisoned")
    }

    /// Replace the slash menu. Empty closes it.
    ///
    /// Set by whoever owns the command registry — the front end — because the
    /// menu's *contents* are a question about commands, not about the screen.
    /// Where it is drawn is this struct's business, not the caller's.
    ///
    /// **The cursor resets to the first row**, and that is the point: a list
    /// that narrowed under a typing hand has a different row 0 than it did a
    /// keystroke ago, and a cursor carried across that is pointing at whatever
    /// happens to be in that slot now. The requirement — a menu opens with its
    /// first row lit — is this line.
    pub fn set_menu(&self, items: Vec<crate::menu::Item>) {
        *self.menu.write().expect("menu poisoned") = crate::menu::Slash::new(items);
    }

    /// Whether the slash menu has anything to offer.
    pub fn menu_open(&self) -> bool {
        !self.menu.read().expect("menu poisoned").is_empty()
    }

    /// What the slash menu has lit, as it would be handed over.
    ///
    /// One reader for the return key, tab, and a click on a row, so completing
    /// and running cannot disagree about which command is meant. Nothing lit —
    /// the list is empty — is `None`, and the caller falls through to the
    /// ordinary keys.
    pub fn menu_selected(&self) -> Option<String> {
        self.menu
            .read()
            .expect("menu poisoned")
            .selected()
            .map(|i| i.value.clone())
    }

    /// Move the slash menu's cursor. Returns whether a frame is owed.
    pub fn menu_move_by(&self, delta: i32) -> bool {
        let mut menu = self.menu.write().expect("menu poisoned");
        if menu.is_empty() {
            return false;
        }
        menu.move_by(delta)
    }

    /// Close the slash menu, if it is open. Returns whether there was one.
    ///
    /// Closing is *not* clearing the registry — the next keystroke recomputes
    /// the list from what is typed, which is why the caller can put it away
    /// without anything having to put it back.
    pub fn close_menu(&self) -> bool {
        let mut menu = self.menu.write().expect("menu poisoned");
        let had = !menu.is_empty();
        *menu = crate::menu::Slash::default();
        had
    }

    /// The pointer moved over the slash menu: light the row it is over.
    ///
    /// `false` when the menu is closed or the pointer is not on it, so the
    /// caller can tell a move that changed the picture from one that did not.
    pub fn menu_hover(&self, x: u16, y: u16) -> bool {
        let rect = match self.hits.lock().expect("hits poisoned").menu {
            Some(rect) => rect,
            None => return false,
        };
        let mut menu = self.menu.write().expect("menu poisoned");
        if menu.is_empty() {
            return false;
        }
        let rows = (rect.h as usize).saturating_sub(1);
        menu.hover(x, y, rect, rows)
    }

    /// A pointer press against the slash menu.
    ///
    /// `Some(value)` when the press landed on a row — the caller completes or
    /// runs it, the same as if the return key had been pressed on that row.
    /// `None` when the menu is closed or the press was off it: a press beside
    /// the list is not the list's business, and only the caller knows what else
    /// is under the pointer.
    pub fn menu_click(&self, x: u16, y: u16) -> Option<String> {
        let rect = *self.hits.lock().expect("hits poisoned").menu.as_ref()?;
        let mut menu = self.menu.write().expect("menu poisoned");
        if menu.is_empty() {
            return None;
        }
        let rows = (rect.h as usize).saturating_sub(1);
        menu.click(x, y, rect, rows)
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

    /// Put a line in the conversation, the way a command's answer lands there.
    ///
    /// Not [`Host::say`], which is the tip row: that one fades after a few
    /// seconds, which is right for "saved" and wrong for the answer to something
    /// that took ten seconds and changed what this build can do. A person who
    /// looked away while a plugin installed has to be able to look back and read
    /// what happened.
    pub fn said(&self, text: impl Into<String>, refused: bool) {
        let mut stream = self.stream.write().expect("stream poisoned");
        let mut w = stream.writer("commands");
        w.emit(
            crate::block::Coord::default(),
            std::sync::Arc::new(crate::content::CommandSaid {
                text: text.into(),
                refused,
            }),
        );
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
    /// of the prompt rather than as a second transcript. The cap is a **window**
    /// now rather than a truncation: the list scrolls with its cursor, so a cap
    /// costs no reachability — see [`crate::menu::Slash::window`].
    fn menu_rows(&self, menu: &crate::menu::Slash) -> u16 {
        (menu.len() as u16).clamp(1, MENU_ROWS)
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
    ///
    /// The list's own rows come from [`crate::menu::Slash::render`], which is
    /// also what decides which row is lit and which row a cell is on — one
    /// layout, so the highlight and the pointer cannot disagree. What is left
    /// here is the margin, which is this panel's and not the list's.
    fn menu_lines(&self, rect: Rect, menu: &crate::menu::Slash) -> Vec<Line> {
        let w = rect.w as usize;
        let room = (rect.h as usize).saturating_sub(1);
        let list = Rect::new(rect.x, rect.y, rect.w, room as u16);
        let mut out = menu.render(list, room);
        out.truncate(room);
        // The margin: one blank row of the panel's colour, so the list reads as
        // a surface lifted off the prompt rather than as text floating on it.
        let style = crate::theme::bg(crate::theme::Role::PanelBg)
            .under(crate::theme::fg(crate::theme::Role::PanelFg));
        out.push(Line::styled(" ".repeat(w), style).truncate(w));
        out
    }

    /// The tail, with its total held below the pane it has to fit in.
    ///
    /// The geometry hands each module its **visible** height and lets it lay
    /// itself out. That is right when the tail is smaller than the pane and
    /// wrong when it is not: a module taller than the pane is asked for the
    /// whole pane at several different offsets and draws the same picture every
    /// time, so the reader turns the wheel and the screen does not move while
    /// the badge counts up. Capping the total leaves at least one row for the
    /// conversation, which is what makes every scroll position distinct.
    ///
    /// The cap is here rather than inside the renderers because it has to be the
    /// *same* number in the geometry and in `stream_height`: two caps that
    /// disagreed would put the tail's own rows out of reach at the bottom of the
    /// scroll, which is the failure `stream_height` goes to lengths to avoid.
    ///
    /// Rationed **top-down**: the first id gives up its rows first, so the one
    /// against the bottom edge — the newest thing, and the one that answers "is
    /// it stuck?" — keeps its rows longest.
    ///
    /// This was written backwards once: the comment said the bottom kept its
    /// rows and the loop took them from the bottom first. On a long plan in a
    /// short terminal that meant the live line was the first thing to go, which
    /// is the worst of the two to lose — the task list has a `window` for
    /// running out of room, and the live line has nothing to fall back on.
    fn cap_tail(mut heights: Vec<(String, u16)>, pane_h: u16) -> Vec<(String, u16)> {
        // One row for the conversation, at least. A pane of one row cannot hold
        // a tail and a conversation both, and the conversation is what the
        // screen is for.
        let budget = pane_h.saturating_sub(1);
        let mut total: u16 = heights.iter().map(|(_, h)| *h).sum();
        // Forward, not reversed: `heights` is in layout order, top to bottom.
        for (_, h) in heights.iter_mut() {
            if total <= budget {
                break;
            }
            let take = total.saturating_sub(budget).min(*h);
            *h -= take;
            total -= take;
        }
        heights
    }

    /// The pane a `Stream` was given, split between the conversation and the
    /// view modules riding at its tail.
    ///
    /// `scroll` is measured from the **bottom of the whole pane**, which is what
    /// makes the tail get out of the way first: its rows are the newest content
    /// there is, so they are what a reader scrolling back has already read. Only
    /// once the tail is gone does `block_scroll` leave zero and the conversation
    /// start moving.
    ///
    /// The arithmetic, with `B` block rows and `T` tail rows in a pane of `H`:
    ///
    /// ```text
    /// visible_tail = (T - scroll).clamp(0, H)
    /// block_scroll = (scroll - T).max(0)
    /// scroll_limit = (B + T) - H
    /// ```
    ///
    /// `tail` is empty for every layout that declares none, and then this is the
    /// identity: the whole pane is the block rect and `block_scroll == scroll`.
    fn pane_geometry(pane: Rect, scroll: usize, heights: &[(String, u16)]) -> Pane {
        Self::pane_geometry_capped(pane, scroll, &Self::cap_tail(heights.to_vec(), pane.h))
    }

    fn pane_geometry_capped(pane: Rect, scroll: usize, heights: &[(String, u16)]) -> Pane {
        // Everything below is in "distance from the bottom of the pane", which
        // is the coordinate `scroll` is already measured in: `d = 0` is the last
        // row of content. The window shows `[scroll, scroll + h)`.
        let top = scroll.saturating_add(pane.h as usize);
        let tail_full: usize = heights.iter().map(|(_, h)| *h as usize).sum();
        let visible_tail = tail_full.saturating_sub(scroll).min(pane.h as usize) as u16;

        // Walking bottom-up: the ids arrive top-to-bottom, so the last one is
        // the lowest on screen — the newest thing where the eye already is.
        let mut offset = 0usize;
        let mut tail: Vec<(String, Rect)> = Vec::new();
        for (id, h) in heights.iter().rev() {
            let (lo, hi) = (offset, offset + *h as usize);
            // The part of this module the window still covers. A module wholly
            // above the fold contributes nothing and is not placed; one caught
            // half-way keeps its visible rows and the rect is that much shorter,
            // which is the whole reason the module is asked for its own height
            // rather than for the band it would have had.
            let vis_lo = lo.max(scroll);
            let vis_hi = hi.min(top);
            if vis_hi > vis_lo {
                // `d = scroll` is the pane's bottom row, so a module ending at
                // `vis_hi` starts `vis_hi - scroll` rows above that bottom edge.
                let y = pane.y + pane.h - (vis_hi - scroll) as u16;
                tail.push((
                    id.clone(),
                    Rect::new(pane.x, y, pane.w, (vis_hi - vis_lo) as u16),
                ));
            }
            offset += *h as usize;
        }
        tail.reverse();

        Pane {
            block_rect: Rect::new(pane.x, pane.y, pane.w, pane.h.saturating_sub(visible_tail)),
            block_scroll: scroll.saturating_sub(tail_full),
            tail,
        }
    }

    /// Render the stream's tail into `rect`.
    ///
    /// Blocks are rendered newest-first until the rect is full, then reversed —
    /// so the cost is O(what fits), not O(the conversation) — and what is left
    /// over goes **below** the content rather than above it.
    ///
    /// That last part is a choice, and it is the one a terminal makes: a new
    /// shell prints at the top of the screen and grows downward, and
    /// `atomcode-tuix` does the same (its footer "sits directly below the last
    /// body row, not pinned to the screen bottom"). It used to be the other way
    /// round here — short content pushed to the foot of the pane — which put a
    /// session's opening block against the input box with the blank rows above
    /// it, reading as a screen *ending* rather than one that has just started.
    ///
    /// Turning the padding around does not turn the *scroll* around: with more
    /// content than room the rect is full and there is no padding either way,
    /// and under that threshold `scroll_limit` is zero (see
    /// [`Self::scroll_limit`]), so the reader is pinned to the bottom of
    /// nothing. The newest line stops being the last row on screen exactly when
    /// there is no screenful of conversation yet.
    ///
    /// `scroll` is passed rather than read: the caller is the one that knows how
    /// the pane was split (`pane_geometry` above gives it the blocks' own
    /// share), and a second read of `moment.scroll` here would be a second
    /// answer to a question someone already answered.
    fn stream_lines(
        &self,
        rect: Rect,
        scroll: usize,
        caps: crate::block::ShapeCaps,
        activity: crate::moment::Activity,
    ) -> (Vec<Line>, Vec<RowOwner>) {
        let stream = self.stream.read().expect("stream poisoned");
        let pres = self.presentation.read().expect("presentation poisoned");
        let mut out: Vec<Line> = Vec::new();
        // Grown in lockstep with `out`, so a row and its owner cannot get out
        // of step — the alternative is two loops that agree until one changes.
        let mut owner: Vec<RowOwner> = Vec::new();
        let want = rect.h as usize;

        // A question in flight *used* to be drawn here, unconditionally. It is a
        // module riding the tail now ([`crate::modules::ask`]), which is where it
        // belongs for the same reason the live line and the steering bars are: it
        // is the newest thing on screen, it has no answer yet so it is not a
        // block, and it should take its room from the pane in the one place that
        // knows how to split one.
        //
        // Drawn here **only when that panel is not mounted** — `Moment::asking` is
        // what says so, and the tail places nothing for a module that is not
        // there. So this is now the fallback rather than a second copy: the plain
        // lines every screen can draw, for the screen that removed the row. That
        // is what makes the row a product's decision instead of a dependency of
        // the front end.
        if !self.ask_panel_mounted() {
            if let Some((_, question)) = self.asks.peek() {
                let pending = crate::content::ChoiceBlock {
                    question: crate::ask::recorded(&question),
                    options: question
                        .options
                        .iter()
                        .map(|a| crate::ask::answer_label(&a.value, &a.label))
                        .collect(),
                    answer: None,
                };
                let mut lines =
                    crate::block::Content::lines(&pending, &crate::block::RenderCtx::bare(rect.w));
                lines.reverse();
                for line in lines {
                    if out.len() < want {
                        out.push(line);
                        owner.push(None); // a question is not a block yet
                    }
                }
            }
        }
        // The same table `stream_height` summed, so the two cannot disagree
        // about how tall anything is. It carries the run table too, so the walk
        // below never builds `lids` of its own — that fold is O(slots) and the
        // index already paid for it.
        let index = self.row_index(
            &crate::block::RenderCtx {
                width: rect.w,
                caps,
            },
            stream.slots(),
            &pres,
            activity,
        );

        // **Start where the window starts.** Everything newer than this is
        // wholly inside the rows the reader has scrolled past, so stepping
        // through it one slot at a time only buys iterations: on a 1570-slot
        // session that was 5.25 of a 5.37ms frame at the top of the scroll
        // (release, 2026-09-15) for a window 24 rows tall. `jump_to` is a
        // partition point over the prefix sum the index already keeps.
        #[cfg(test)]
        let no_jump = crate::host::NO_JUMP.load(std::sync::atomic::Ordering::SeqCst);
        #[cfg(not(test))]
        let no_jump = false;
        let (start, mut skipped, mut below) = if no_jump {
            (
                stream.slots().len().saturating_sub(1),
                0usize,
                None::<&'static str>,
            )
        } else {
            index.jump_to(scroll)
        };
        // Skipped only in the sense of "not walked": `start` is the slot the
        // window begins in, and the loop below decides the exact boundary from
        // here the same way it always did.
        let walked = stream.slots().len().saturating_sub(start + 1);

        for (i, slot) in stream.slots().iter().enumerate().rev().skip(walked) {
            // Everything this walk needs to decide *whether* to draw came out of
            // the index while it was built: a `None` is a slot the frame draws
            // nothing for — off-screen by kind, empty, or an earlier member of a
            // merged run whose lid the walk below reached first. Asking the
            // block again here was four virtual calls and several string
            // comparisons per slot per frame.
            let Some(entry) = index.rows.get(i).copied().flatten() else {
                continue;
            };
            let block = slot.block();
            let kind = entry.kind;
            let lid = entry.lid;
            // Two different questions. Reasoning and tool calls both fold — that
            // is what ctrl-r and ctrl-t are — but what a click may fold is
            // narrower than what folds: prose is out, because it is what the
            // transcript is for and most of the screen is things the model said.
            // Whether it *can* fold — a property of the content, not of whether
            // it happens to be folded right now. Conflating the two made an
            // already-folded block refuse a click that had just folded it: the
            // click worked once and then the row stopped answering.
            let foldable = !block.content.always_open();
            let mut own = (foldable && CLICKABLE.contains(&kind)).then_some((block.id, kind));
            // The column this block leaves on the left, and the width that is
            // actually left to draw in. Every render below is asked for `room`,
            // never `rect.w` — content wrapped to the full width and then set in
            // would overflow, and the cut in `set_in_row` would eat its last
            // cells.
            let pad = inset(kind);
            let room = rect.w.saturating_sub(pad);
            // One ctx for this block, at the width it is drawn at and the
            // capabilities this frame was composed against. The same two values
            // `row_index` measured it under, so the count and the picture agree.
            let ctx = crate::block::RenderCtx { width: room, caps };
            let lines: Arc<Vec<Line>> = if let Some(count) = lid {
                // A run of folded calls behind one lid. Its rows are owned by
                // the last call, so a click anywhere on the lid folds the run
                // that drew it — which is the only thing that click could mean.
                own = Some((block.id, kind));
                Arc::new(lid_lines(
                    stream.slots(),
                    i,
                    count,
                    entry.lid_failed,
                    entry.lid_live,
                    room,
                ))
            } else if entry.undone {
                // Dimmed as well as folded: it is still there to read, and it
                // is no longer what the model sees.
                let rows = block
                    .content
                    .summary_lines(&crate::block::RenderCtx { width: room, caps });
                Arc::new(
                    rows.into_iter()
                        .map(|line| {
                            let width = line.width();
                            line.restyle(0, width, |_| crate::theme::fg(crate::theme::Role::Muted))
                        })
                        .collect(),
                )
            } else if entry.folded {
                Arc::new(block.content.summary_lines(&ctx))
            } else {
                // The count comes from the index — the same number
                // `stream_height` summed — rather than from a second measurement
                // behind the block's own write lock. That second measurement was
                // one lock acquisition per slot per frame, and on a 1458-slot
                // session it was most of the 13.7ms this path cost per wheel
                // notch (measured, debug, 2026-09-15).
                //
                // With the count in hand, a block entirely above the reader is
                // skipped in arithmetic instead of being rendered and thrown
                // away, which is what makes scrolling back through a long
                // conversation cost the screen rather than the session.
                let n = entry.rows;
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
                // Rendered only now, because the count above proved the reader
                // can see this block — and rendering is the one part of this walk
                // that cannot come from a table. Still through `rows_at`, so a
                // growing answer extends its live cache in place.
                let lines = match slot.rows_at(&ctx).1 {
                    Some(lines) => lines,
                    None => Arc::new(block.content.lines(&ctx)),
                };
                // A Head preview clips the drawing to the two ends, with the
                // muted fold note between them — the row the index counted as
                // one of `head_rows`, so the count and the picture agree. The
                // note belongs to the block (a click on it opens the call in
                // full), and its pale background is what makes it read as a
                // seam in the call rather than as output.
                if entry.preview {
                    let mut clipped: Vec<Line> = lines[..HEAD_ROWS.min(lines.len())].to_vec();
                    let hidden = lines.len() - 2 * HEAD_ROWS;
                    // Muted text on the panel ground: a seam in the call, not
                    // output — and quiet enough to read past.
                    let note = Span::styled(
                        crate::i18n::t(crate::i18n::Msg::FoldedLines { hidden }).into_owned(),
                        crate::theme::fg(crate::theme::Role::Muted)
                            .bg(crate::frame::Color::role(crate::theme::Role::PanelBg)),
                    );
                    clipped.push(Line::from_spans(vec![note]).truncate(room as usize));
                    clipped.extend(lines[lines.len() - HEAD_ROWS..].to_vec());
                    Arc::new(clipped)
                } else {
                    lines
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
        // What is left over is blank rows **under** the content, so the
        // conversation starts at the top of the pane and grows down into them.
        // See this function's own doc for why, and for why this is not the same
        // question as which end the *scroll* is measured from.
        let pad = want.saturating_sub(out.len());
        out.extend(vec![Line::empty(); pad]);
        owner.extend(vec![None; pad]);
        (out, owner)
    }

    /// Compose one frame.
    pub fn compose(&self, size: (u16, u16)) -> Frame {
        let (w, h) = size;
        let mut frame = Frame::new(w, h);
        let mut moment = self.moment.read().expect("moment poisoned").clone();
        // This frame's bitmaps, as a snapshot. Here because `View::render` cannot
        // reach a service — the module reads `viewport.moment.rasters`, the same
        // road `members` travels. One `Arc` bump per frame: the map is rebuilt on
        // a write, not on a draw.
        moment.rasters = self.rasters.view();
        // The shape half of what this terminal can draw, taken once from the
        // moment this frame was composed against and handed down. Built here
        // rather than read inside `rows_at` because `stream_height`'s contract is
        // that the caller may already hold the moment's write lock (two callers
        // in `plugin.rs` do), so nothing below may take it again.
        let caps = crate::block::ShapeCaps::of(&moment.caps);

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
                Region::Stream { tail } => {
                    // The pane is split before anything is drawn: the modules
                    // riding the tail take their rows off the bottom, and what
                    // is left is the conversation's. With no tail declared this
                    // is the identity — one rect, the offset unchanged — which
                    // is why every layout in this build still composes the same
                    // frame it did before the split existed.
                    let heights = self.tail_heights_of(&tail, rect.w, &moment);
                    let pane = Self::pane_geometry(rect, moment.scroll.0, &heights);
                    let (lines, owners) = self.stream_lines(
                        pane.block_rect,
                        pane.block_scroll,
                        caps,
                        moment.activity,
                    );
                    *self.hits.lock().expect("hits poisoned") = Hits {
                        rect: pane.block_rect,
                        rows: owners,
                        jump: None,
                        field: None,
                        ask: None,
                        team: None,
                        settings: None,
                        providers: None,
                        plugins: None,
                        tools: None,
                        rewind: None,
                        resume: None,
                        menu: None,
                    };
                    *self.last_room.lock().expect("room poisoned") = rect;
                    // The **blocks'** rect, not the pane's. The badge reports
                    // how much conversation is out of view, so it belongs on the
                    // conversation's bottom edge; anchored to the pane it would
                    // sit over the tail and cover the right half of its last
                    // row — and the tail is the one thing a reader scrolled back
                    // is *not* looking at.
                    stream_rect = Some(pane.block_rect);
                    frame.place("stream", pane.block_rect, lines);
                    // A tail module is placed under **its own id**, not a
                    // path: `named_twice` already refuses a tree that names one
                    // both as a tail id and as a leaf, so an id is unique in a
                    // frame and everything that finds a module by name —
                    // `part("live")`, the click handler, the tests — keeps
                    // working without knowing where the module sits. The rect is
                    // what the window covers: a module half scrolled out is
                    // asked for its visible height and lays itself out in it,
                    // rather than being sliced.
                    for (id, tail_rect) in &pane.tail {
                        let Some(view) = modules.view(id) else {
                            continue;
                        };
                        let vp = crate::moment::Viewport::new(*tail_rect, &moment);
                        let mut lines = view.render(&vp);
                        lines.truncate(tail_rect.h as usize);
                        // A click inside the question panel has to find the
                        // answer it landed on, and only the host knows where the
                        // panel was put. Recorded for the same reason the
                        // composer's rect is: the press is answered from the
                        // frame that is on screen, not from a formula that would
                        // have to reproduce the tail's split.
                        if id == crate::modules::ask::ID {
                            self.hits.lock().expect("hits poisoned").ask = Some(*tail_rect);
                        }
                        // And the same for the settings panel, which rides the
                        // same tail: where it sits depends on how tall the
                        // modules below it turned out, so the press has to be
                        // answered from this frame's rect rather than from a
                        // formula that re-splits the tail.
                        if id == crate::modules::settings::ID {
                            self.hits.lock().expect("hits poisoned").settings = Some(*tail_rect);
                        }
                        // And the providers panel, which rides the same tail and
                        // is worked with the same pointer.
                        if id == crate::modules::providers::ID {
                            self.hits.lock().expect("hits poisoned").providers = Some(*tail_rect);
                        }
                        // And the plugins panel, which rides the same tail.
                        if id == crate::modules::plugins::ID {
                            self.hits.lock().expect("hits poisoned").plugins = Some(*tail_rect);
                        }
                        // And the tools panel.
                        if id == crate::modules::tools::ID {
                            self.hits.lock().expect("hits poisoned").tools = Some(*tail_rect);
                        }
                        // And the rewind panel.
                        if id == crate::modules::rewind::ID {
                            self.hits.lock().expect("hits poisoned").rewind = Some(*tail_rect);
                        }
                        // And the resume panel, worked with the same pointer.
                        if id == crate::modules::resume::ID {
                            self.hits.lock().expect("hits poisoned").resume = Some(*tail_rect);
                        }
                        frame.place(id.clone(), *tail_rect, lines);
                    }
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
                    if id == crate::modules::team::ID {
                        self.hits.lock().expect("hits poisoned").team = Some(rect);
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
        //
        // Its rect is left behind in `hits` for the same reason the question
        // panel's is: a pointer has to be answered from the picture that was on
        // screen. The panel hangs off the field's top edge and its window
        // scrolls with the cursor, so re-deriving either at the press is a
        // second layout to keep in step with this one.
        let menu = self.menu.read().expect("menu poisoned").clone();
        if !menu.is_empty() {
            if let Some(rect) = self.menu_rect(&frame, Rect::sized(w, h), self.menu_rows(&menu)) {
                frame.place("menu", rect, self.menu_lines(rect, &menu));
                self.hits.lock().expect("hits poisoned").menu = Some(rect);
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
            let label = crate::i18n::t(crate::i18n::Msg::MoreBelow {
                arrow: caps.g(crate::caps::Glyph::Down),
                lines: moment.scroll.0,
            })
            .into_owned();
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
        self.stream_room(size, moment).h
    }

    /// The box the conversation gets on a screen of `size`.
    ///
    /// **The only place a stream's size is worked out**, and it is a `Rect`
    /// rather than a pair of numbers because both dimensions matter and they
    /// are used for different things: the width is what every block is
    /// *measured* at, and the height is what the tail has to share with it.
    ///
    /// Taking the width from the screen instead is the mistake this exists to
    /// stop. Under `wide` the conversation is 65% of the screen, so a block
    /// measured at the full width wraps into fewer rows than the frame draws —
    /// and a reader scrolled back drifts by the difference, one chunk at a
    /// time.
    fn stream_room(&self, size: (u16, u16), moment: &Moment) -> Rect {
        let (w, h) = size;
        let modules = self.modules.clone();
        let pruned = self.layout.tree().prune(&|id| modules.has_view(id));
        let asked = |id: &str| -> u16 { asked_height(&modules, id, moment, w) };
        pruned
            .layout_with(Rect::sized(w, h), &asked)
            .into_iter()
            .find_map(|(region, rect)| matches!(region, Region::Stream { .. }).then_some(rect))
            .unwrap_or_else(|| Rect::sized(w, h))
    }

    /// How far back the stream can be scrolled, in rendered lines. Zero when
    /// everything there is to read is already on screen.
    /// The moment is an argument, not a field read: the caller is holding the
    /// write lock on it when it asks, and a `RwLock` does not forgive that.
    pub fn scroll_limit(&self, size: (u16, u16), moment: &Moment) -> usize {
        self.stream_height(size, moment)
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
    pub fn stream_height(&self, size: (u16, u16), moment: &Moment) -> usize {
        self.stream_height_in(self.stream_room(size, moment), moment)
            .0
    }

    /// Throw the index away, so the next ask rebuilds it from the stream.
    ///
    /// Test-only, and the whole point of the ratchet beside it: the property
    /// that needs proving is that the incrementally-kept index says exactly what
    /// a from-scratch one would. Without a way to force the rebuild there is
    /// nothing to compare the reuse against.
    #[cfg(test)]
    fn forget_row_index(&self) {
        self.row_index.lock().expect("row index poisoned").width = 0;
    }

    /// The direct walk, kept as the ratchet's reference.
    ///
    /// This is what the frame used to do per call: for every slot, ask the
    /// presentation whether it is hidden, ask the run table whether a lid covers
    /// it, and measure the block. It is correct and it is O(slots) of locks and
    /// folds — which is why it cost 17.7ms a wheel notch on a 1330-slot session.
    ///
    /// Test-only rather than deleted, because "the index agrees with the walk"
    /// is the only statement that makes the index safe to trust, and a copy of
    /// the walk kept anywhere else would be a second answer rather than a
    /// yardstick.
    #[cfg(test)]
    fn rows_by_walk(
        &self,
        ctx: &crate::block::RenderCtx,
        slots: &[crate::block::Slot],
        pres: &Presentation,
    ) -> Vec<Option<SlotRows>> {
        let lids = lids(slots, pres, crate::moment::Activity::Working);
        (0..slots.len())
            .map(|i| {
                let b = slots[i].block();
                let room = ctx.width.saturating_sub(inset(b.kind()));
                lid_row(
                    &lids,
                    slots,
                    i,
                    &crate::block::RenderCtx {
                        width: room,
                        ..*ctx
                    },
                    b,
                    pres,
                )
            })
            .collect()
    }

    /// Build or extend the row index, and hand it back.
    ///
    /// **One row sum for the whole host.** `stream_height_in` bounds the scroll
    /// with it and `stream_lines` fills the viewport from it, so the two cannot
    /// disagree about how tall anything is — the failure the old duplicate walks
    /// could only avoid by being kept identical by hand.
    ///
    /// Incremental in the two directions that matter, which are different
    /// cadences:
    ///
    /// * **A scroll** changes neither revision, so this returns what it built
    ///   last frame and the walks below cost an array read per slot instead of a
    ///   lock, a hashmap and a fold lookup.
    /// * **A streamed chunk** amends the trailing live block without touching the
    ///   stream revision (`amend` is deliberately not a bump — see `Stream`), so
    ///   only that one slot is re-measured. Everything settled keeps the count it
    ///   settled with, which is the promise settling makes.
    ///
    /// A different width or a different idea of what is folded changes the
    /// answer for every slot, so those start over. Both are revisions rather
    /// than arguments somebody passes correctly: see `Presentation::revision`.
    fn row_index<'a>(
        &'a self,
        ctx: &crate::block::RenderCtx,
        slots: &[crate::block::Slot],
        pres: &Presentation,
        activity: crate::moment::Activity,
    ) -> std::sync::MutexGuard<'a, RowIndex> {
        let mut idx = self.row_index.lock().expect("row index poisoned");
        if idx.width != ctx.width
            || idx.caps != ctx.caps
            || idx.presentation != pres.revision()
            || idx.activity != activity
        {
            idx.measured.clear();
            idx.rows.clear();
            idx.width = ctx.width;
            idx.caps = ctx.caps;
            idx.presentation = pres.revision();
            idx.activity = activity;
        }
        let lids = lids(slots, pres, activity);
        for i in 0..slots.len() {
            let slot = &slots[i];
            let b = slot.block();
            let settled = slot.is_settled();
            // Reuse only what settling froze at this exact width. A live block
            // can have grown since, and that is the whole reason it is not
            // cached.
            //
            // A slot of a tool run is never reused, settled or not: whether it
            // draws a lid, and how many calls that lid names, is a fact about
            // the *run* — and the run changes shape when a call lands in it
            // without this slot's id changing at all. The lid the run used to
            // draw here would ride its cached entry past the new lid drawn at
            // the run's new last call, and the screen would show both. Tool
            // slots are a small fraction of the stream; the prose this table
            // exists to skip is untouched.
            let in_a_run = lids.runs.get(i).copied().flatten().is_some();
            let reusable = settled
                && !in_a_run
                && idx
                    .measured
                    .get(i)
                    .is_some_and(|m| m.settled && m.id == b.id);
            if reusable {
                continue;
            }
            // The same width the painter will use: a row count is only the
            // painter's if it was measured at the width the painter draws at,
            // and a block that is set in draws two cells narrower.
            let room = ctx.width.saturating_sub(inset(b.kind()));
            // Kept as `Option`, not folded to zero. A slot that draws nothing is
            // not a row AND not a neighbour — the seams around it stay where
            // they were — and collapsing the two made a hidden block add itself
            // to the running kind, which moved every blank below it. The ratchet
            // beside this caught exactly that on its first run.
            let entry = lid_row(
                &lids,
                slots,
                i,
                &crate::block::RenderCtx {
                    width: room,
                    ..*ctx
                },
                b,
                pres,
            );
            let m = Measured { id: b.id, settled };
            if idx.measured.len() <= i {
                idx.measured.push(m);
                idx.rows.push(entry);
            } else {
                idx.measured[i] = m;
                idx.rows[i] = entry;
            }
        }
        // The suffix sum, in the walk's terms — see `skip_from`. Backwards, so
        // the slot seen last is the one *below* the current one, which is the
        // seam the newest-first walk charges to this slot.
        let mut below: Option<&'static str> = None;
        let mut acc = 0usize;
        idx.skip_from.clear();
        idx.skip_from.resize(slots.len() + 1, 0);
        for i in (0..slots.len()).rev() {
            if let Some(entry) = idx.rows.get(i).copied().flatten() {
                if entry.rows > 0 {
                    let seam = below.is_some_and(|b| blank_between(entry.kind, b));
                    acc += entry.rows + usize::from(seam);
                    below = Some(entry.kind);
                }
            }
            idx.skip_from[i] = acc;
        }
        idx.total = acc;
        idx
    }

    /// [`Host::stream_height`] for a caller that already knows the box.
    ///
    /// The frame worked it out to lay the regions out, and the pin is holding
    /// the one from the frame it is pinning against; asking again would be a
    /// second answer to a question both of them already have — and the two
    /// would disagree on the frame where the layout changed under them.
    ///
    /// Returns the total and the tail's share together, because the frame needs
    /// both: the total bounds the scroll, and the tail's share is what decides
    /// where the conversation ends and the tail begins. Two walks would agree
    /// until one of them changed.
    fn stream_height_in(&self, room: Rect, moment: &Moment) -> (usize, usize) {
        let width = room.w;
        let caps = crate::block::ShapeCaps::of(&moment.caps);
        let stream = self.stream.read().expect("stream poisoned");
        let pres = self.presentation.read().expect("presentation poisoned");
        // The sum, from the one place both this and the painter read it — see
        // [`Host::row_index`]. This used to walk every slot itself: a second
        // answer to a question the frame had already asked. On a 1458-slot
        // session the two walks cost 4.1ms here and 13.7ms in `stream_lines`,
        // per wheel notch (measured, debug, 2026-09-15).
        let total = self
            .row_index(
                &crate::block::RenderCtx { width, caps },
                stream.slots(),
                &pres,
                moment.activity,
            )
            .total;
        // The tail is content too, so it counts towards what there is to read:
        // leaving it out would put its own rows out of reach at the bottom of
        // the scroll, which is exactly the failure the block walk goes to such
        // lengths to avoid one paragraph up.
        //
        // The same cap the geometry uses, and the same height it uses it at.
        // This is the one place that would otherwise count rows the frame
        // declined to draw, putting them out of reach at the bottom of the
        // scroll.
        let tail: usize = Self::cap_tail(self.tail_heights(width, moment), room.h)
            .iter()
            .map(|(_, h)| *h as usize)
            .sum();
        // The question drawn at the foot of the stream when no panel is mounted —
        // see `stream_lines`. Counted here for the reason the tail is: a row the
        // frame drew and this did not would be out of reach at the bottom of the
        // scroll, which is the whole failure `row_index` exists to prevent.
        let pending = match self.ask_panel_mounted() {
            false => self.asks.peek().map(|(_, q)| q),
            true => None,
        };
        let question = pending.map_or(0, |q| {
            let block = crate::content::ChoiceBlock {
                question: crate::ask::recorded(&q),
                options: q
                    .options
                    .iter()
                    .map(|a| crate::ask::answer_label(&a.value, &a.label))
                    .collect(),
                answer: None,
            };
            crate::block::Content::lines(&block, &crate::block::RenderCtx::bare(width)).len()
        });
        (total + tail + question, tail)
    }

    /// The view modules riding the stream's tail, with the height each asks for
    /// at this width — the declaration order of the layout, top to bottom.
    ///
    /// Empty when the layout declares no tail, which is every layout in this
    /// build until ADR 0020's Step 3: the pane is then the blocks' alone and the
    /// geometry is the identity.
    ///
    /// The height comes from [`ViewObject::height`](crate::module::ViewObject::height),
    /// which is a number the module computes rather than a render it does — a
    /// tail costs an arithmetic call per frame, not a second render of itself.
    fn tail_heights(&self, width: u16, moment: &Moment) -> Vec<(String, u16)> {
        let ids = self.layout.tree().tail().to_vec();
        self.tail_heights_of(&ids, width, moment)
    }

    /// [`Host::tail_heights`] for a caller that already holds the ids.
    ///
    /// The frame has them — they came out of the `Region::Stream` it is
    /// composing. Reading the tree again would be a second answer, and the two
    /// can differ: a panel row mounting on another thread between the layout
    /// and the compose would have this frame draw the old arrangement's tail
    /// while the flex above placed the new one's.
    fn tail_heights_of(&self, ids: &[String], width: u16, moment: &Moment) -> Vec<(String, u16)> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(view) = self.modules.view(id) else {
                // Not mounted: `prune` would have taken it off the tree, and a
                // tail id that is not mounted is not a row of anything.
                continue;
            };
            let h = match view.height(moment, width) {
                Height::Fixed(n) | Height::Hug(n) => n,
                Height::Fill => 1,
            };
            out.push((id.clone(), h));
        }
        out
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
    ///
    /// In the Head mode a call has three shapes rather than two, and the click
    /// walks them: preview → full → folded → preview. The two per-call maps
    /// (`by_block`, `full_open`) are written so the next shape follows from the
    /// current one — a previewed call opens whole, a whole one folds, a folded
    /// one comes back as the preview the mode draws by default.
    pub fn toggle_block(&self, id: BlockId, kind: &str) {
        let stream = self.stream.read().expect("stream poisoned");
        let slots = stream.slots();
        // Read first and let the guard go: the run is asked of the state a click
        // was answered against, and the write below needs the lock to itself.
        let (run, show) = {
            let pres = self.presentation.read().expect("presentation poisoned");
            (run_around(slots, &pres, id), pres.tool_show(id))
        };
        let mut pres = self.presentation.write().expect("presentation poisoned");
        if kind == "tool_call" && pres.tool_output == ToolOutput::Head {
            for block in run {
                match show {
                    ToolShow::Preview => {
                        pres.by_block.remove(&block);
                        pres.full_open.insert(block);
                    }
                    ToolShow::Full => {
                        pres.full_open.remove(&block);
                        pres.by_block.insert(block, true);
                    }
                    ToolShow::Folded => {
                        pres.by_block.remove(&block);
                    }
                }
            }
            pres.bump();
            return;
        }
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
/// The modules the composer is made of, and the ones a question displaces.
///
/// [`composer`] builds its tree from this list, so "what the composer is" and
/// "what a question makes room for" are one answer rather than two that agree
/// until a row is added to one of them. `live` is deliberately not a member: it
/// rides the tail and steps aside in its own `height`, where its own visibility
/// already lives.
pub const COMPOSER: &[&str] = &[crate::modules::tip::ID, crate::modules::input::ID];

fn asked_height(modules: &Modules, id: &str, moment: &Moment, width: u16) -> u16 {
    let asked = modules
        .view(id)
        .map(|v| match v.height(moment, width) {
            Height::Fixed(n) | Height::Hug(n) => n,
            Height::Fill => 1,
        })
        .unwrap_or(1);

    // A question waiting, or the settings panel up, takes the composer's place,
    // and the composer gives it up here rather than in its own `height`.
    //
    // `height` is a question about the *module* — how many rows does this prompt
    // field need for the text in it — and the field needs the same rows whether
    // or not a panel is up. What changes is what the screen does with them, and
    // that is arbitration: the host's, by the same rule that clips a module
    // asking for too much. A module that returned zero here because something
    // else on screen is asking would be a module whose own size depends on a
    // sibling, which is the thing the tail's `Hug` contract exists to keep out.
    //
    // The composer as a whole, not just the field: `tip`'s reserved row is part
    // of it, and a blank row left above a panel is the shadow of a box that is
    // not there.
    //
    // **One predicate, asked once.** Two `is_some()` checks here would agree
    // until a third panel was added and one of them was not, and the symptom
    // would be a composer drawn *under* a panel that covers it.
    if displaces_composer(moment) && COMPOSER.contains(&id) {
        return 0;
    }
    asked
}

/// Whether what is on screen stands in the composer's place.
///
/// The one question [`asked_height`] arbitrates on, named so that the two
/// things that can answer yes — a question, and a panel a person works in — are
/// listed in a single place. A caller that asked them separately would be a second
/// answer to one question, and the two would part company the day a third panel
/// was added.
pub fn displaces_composer(moment: &Moment) -> bool {
    // Nothing displaces it while a password is being asked for, and that is not
    // a courtesy: the password is typed **in the field**, so a panel that took
    // the composer's rows would take the prompt off the screen with them — and
    // the keys still go to it (focus is decided in one place, and a password a
    // blocked process is waiting on comes first there). A question and a `sudo`
    // can be up at once whenever two tools run in one batch; the panel keeps its
    // own rows, and both are on screen.
    if moment.secret.is_some() {
        return false;
    }
    moment.asking.is_some() || moment.settings_panel.is_some() || moment.providers_panel.is_some()
}

/// The view modules whose rows ride at the foot of the conversation.
///
/// **One list, in one place.** Every tree that builds a scroll region goes
/// through [`scroll_region`], and a tree that forgot one would not be caught by
/// the check that refuses a module named twice: naming it *nowhere* is not
/// naming it twice. So the list is added to once or not at all.
///
/// What belongs here is "what this turn is working through", which is worth
/// scrolling back for. What does not is the frame — the input box, the tip row,
/// the status line: those say where the session *is*, have no history, and must
/// not move out from under a hand reaching for them. See `docs/adr/0020`.
///
/// The steering panel is last, under the live line: it is the newest thing on
/// screen (words typed seconds ago, not yet sent) and the tail is laid out from
/// the bottom up, so last means closest to the composer the person just typed
/// into.
///
/// **`ask` sits between the live line and the steering bars**, and it is the one
/// member that is not about what the turn is doing: it is about what the turn is
/// waiting for. Below the live line, because "still moving" is context for the
/// answer; above the steering bars, because a question is what has to be dealt
/// with and words not yet sent are what happens next. While it is there it is
/// also the only thing on the tail that takes keys.
///
/// **`providers` rides with `settings`**, because it is the same kind of thing —
/// a panel a person edits their configuration in — and only one of the two is
/// ever up: opening either puts the other away ([`Host::toggle_providers`]).
///
/// **`settings` rides here too, and directly under the live line** — above the
/// question, because the two cannot both be up in a way that matters: a question
/// is the model *waiting*, and a person who opened the settings is answering it
/// by doing something else. It is placed like `ask` rather than like the
/// composer because it is the same kind of thing: a panel a hand is working in,
/// which takes keys while it is up and gives them back when it closes.
pub const TAIL: &[&str] = &[
    crate::modules::todo::ID,
    crate::modules::live::ID,
    crate::modules::settings::ID,
    crate::modules::providers::ID,
    crate::modules::plugins::ID,
    crate::modules::tools::ID,
    crate::modules::rewind::ID,
    crate::modules::resume::ID,
    crate::modules::ask::ID,
    crate::modules::steering::ID,
];

/// The conversation, with [`TAIL`] riding at its foot.
///
/// The only way a stream with a tail is built. `El::Stream`'s tail is a list
/// rather than child nodes because the engine must stay ignorant of scrolling
/// (ADR 0020), which leaves the host to split the pane — so this is the host's
/// notion of "the scrollable region", and the trees name it rather than each
/// writing `Region::stream().with_tail(...)` for themselves.
pub fn scroll_region() -> Region {
    Region::stream().with_tail(TAIL.iter().copied())
}

/// The frame: the live line, the reserved tip row, and the field.
///
/// One definition, because every arrangement that has an input has this above
/// it — the input box is where a person's eyes are, and the one thing that
/// answers "is it stuck?" belongs against it rather than in a panel beside the
/// conversation.
///
/// **None of these scroll.** The task list used to be a member here and is not
/// any more; it moved to [`TAIL`], where it scrolls with the conversation
/// instead of standing at the foot of the screen while a reader looks at
/// history. What is left is the part of the screen that is about *now*: moving
/// it would move the field, and the field is what a hand is already reaching
/// for.
///
/// **A question takes the composer's place.** While one is waiting there is
/// nothing to type beside it, so `tip` and `input` ask for no rows and the panel
/// riding the tail is what fills this space. Both are asked for by
/// `asked_height`, which is the one place that decides; the fields are unchanged
/// because the fields are what a screen has, not what a question does to it.
///
/// Written into the tree rather than claimed by each row's own `LayoutOp::Show`
/// the way the mascot claims its strip: `Show` has two sides, above the
/// conversation and below the status line, and both are the wrong side of the
/// input box.
///
/// The tip row is the deliberate exception to `Hug`-ness — it asks for its row
/// always, which is why a tip can never move the box out from under a hand
/// reaching for it. See `modules::tip`. The one exception to that exception is a
/// question, where no hand is reaching for the box.
pub fn composer() -> Region {
    use crate::el::Item;
    use crate::region::Dir;
    // From [`COMPOSER`], because the arbitration that gives these rows up for a
    // question reads the same list: two lists would agree until a row was added
    // to one of them, and the symptom would be a blank row left above a panel.
    let items = COMPOSER
        .iter()
        .map(|id| match *id {
            crate::modules::input::ID => Item::grow(Region::view(*id)),
            _ => Item::hug(Region::view(*id)),
        })
        .collect();
    Region::flex(Dir::Vertical, items)
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
        scroll_region(),
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

    /// Switching the view leaves no `已中断` note or `last_sent` prompt behind
    /// for the conversation that arrives: both belong to the one being left.
    #[test]
    fn a_view_switch_clears_the_interrupted_note_and_the_kept_prompt() {
        let h = host();
        {
            let mut m = h.moment.write().expect("moment poisoned");
            m.interrupted = true;
            m.last_sent = Some("fix the parser".into());
        }
        h.switch_view();
        let m = h.moment.read().expect("moment poisoned");
        assert!(!m.interrupted, "the note does not follow the switch");
        assert_eq!(m.last_sent, None, "nor does the prompt to hand back");
    }

    /// A host with the providers panel's module mounted, as a launcher that
    /// filled the seam gives it.
    fn host_with_providers() -> Host {
        let mods = Arc::new(Modules::new());
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        mods.add_view(Arc::new(
            Mounted::<crate::modules::providers::Providers>::new(),
        ))
        .unwrap();
        Host::new(mods, default_layout())
    }

    /// Two panels a person works in are two claims on one keyboard: the routing
    /// checks them in order, so the second would take the composer's rows and
    /// never see a press. Opening either puts the other away.
    #[test]
    fn only_one_panel_a_person_works_in_is_ever_up() {
        let h = host_with_providers();
        assert!(h.toggle_providers());
        assert!(h.providers_open());
        assert!(h.toggle_settings());
        assert!(!h.providers_open(), "settings put the providers away");
        assert!(h.settings_open());
        assert!(h.toggle_providers());
        assert!(!h.settings_open(), "and the other way round");
        assert!(h.providers_open());
    }

    /// A panel with no module to draw it would take the composer's rows and
    /// every key and show neither, so it is refused instead of opened.
    #[test]
    fn a_screen_without_the_panels_module_refuses_to_open_it() {
        let h = host();
        assert!(!h.toggle_providers());
        assert!(!h.providers_open());
    }

    /// The question a person typed stands apart on BOTH sides — from what came
    /// before it (a previous turn's last line, a compaction notice) and from its
    /// own answer — so the `》` bar is easy to scan back to.
    #[test]
    fn a_user_bar_gets_a_blank_row_above_and_below_it() {
        assert!(
            blank_between("commands", "user"),
            "a blank above the user bar"
        );
        assert!(blank_between("user", "tool_call"), "and below it");
        // Unrelated neighbours still butt together — the rule is the user bar,
        // not a blank between everything.
        assert!(!blank_between("assistant", "assistant"));
    }

    /// Two turns, the second with something long enough to take several rows.
    fn two_turns(h: &Host) {
        let facts = [
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::UserMessage {
                turn: 1,
                text: "the first thing".into(),
                images: Vec::new(),
            },
            SessionEvent::TurnStart { turn: 2 },
            SessionEvent::UserMessage {
                turn: 2,
                text: "the second thing".into(),
                images: Vec::new(),
            },
            SessionEvent::AssistantMessage {
                turn: 2,
                round: 1,
                text: "answer line one\nanswer line two\nanswer line three".into(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            },
        ];
        for (i, fact) in facts.into_iter().enumerate() {
            h.absorb_logged(&LoggedEvent {
                seq: i as u64 + 1,
                at: 0,
                event: fact,
            });
        }
    }

    /// How long since the turn last did anything is stamped where the facts
    /// land — the one place that sees both the fact and the reading.
    ///
    /// Without it the row can only count the turn's own age, which keeps ticking
    /// whether or not anything is arriving: a stalled stream and a working model
    /// read the same.
    #[test]
    fn silence_is_measured_from_the_last_fact_of_the_turn() {
        let h = host();
        let at = |h: &Host, ms: u64| {
            h.moment.write().unwrap().now = crate::moment::Timestamp::millis(ms);
        };
        let quiet_since = |h: &Host| h.moment.read().unwrap().quiet_since;

        assert_eq!(quiet_since(&h), None, "no turn, nothing to measure");
        at(&h, 1_000);
        h.absorb(&SessionEvent::TurnStart { turn: 1 });
        assert_eq!(
            quiet_since(&h),
            Some(crate::moment::Timestamp::millis(1_000))
        );

        // A fact of the turn is a sign of life, whichever fact it is.
        at(&h, 9_000);
        h.absorb(&SessionEvent::AssistantChunk {
            turn: 1,
            round: 1,
            delta: "a".into(),
            reasoning: false,
        });
        assert_eq!(
            quiet_since(&h),
            Some(crate::moment::Timestamp::millis(9_000))
        );

        // And the measure goes with the turn.
        at(&h, 12_000);
        h.absorb(&SessionEvent::TurnEnd {
            turn: 1,
            stop: atomcode_kernel::event::StopReason::Stopped,
            error: None,
        });
        assert_eq!(quiet_since(&h), None);
    }

    /// The working line ("正在等待模型") waits for the turn's first message, so it
    /// never paints a frame above the message that started the turn. The runtime
    /// logs `TurnStart` *before* the user's message, so arming on the turn start
    /// and then folding that structural fact must NOT raise the line — only the
    /// message that follows does.
    #[test]
    fn the_working_line_waits_for_the_turns_first_message() {
        use crate::moment::Activity;
        let h = host();
        assert_eq!(h.moment.read().unwrap().activity, Activity::Idle);

        assert!(!h.arm_working(), "arming draws no frame of its own");
        h.absorb(&SessionEvent::TurnStart { turn: 1 });
        assert_eq!(
            h.moment.read().unwrap().activity,
            Activity::Idle,
            "the spinner must not show before the message"
        );
        assert!(h.moment.read().unwrap().pending_working, "still armed");

        h.absorb(&SessionEvent::UserMessage {
            turn: 1,
            text: "3333333".into(),
            images: Vec::new(),
        });
        assert!(h.settle_working(), "the first message spends the arm");
        assert_eq!(h.moment.read().unwrap().activity, Activity::Working);
        assert!(!h.moment.read().unwrap().pending_working);
    }

    /// A turn that starts without a typed Submit (a scheduled or resumed prompt)
    /// arms through here, not through `Action::Submit` — so the stale `已中断`
    /// note from a turn you stopped earlier must be spent here, or it would hang
    /// under a turn that is now running.
    #[test]
    fn arming_a_turn_spends_a_stale_interrupted_note() {
        let h = host();
        h.moment.write().unwrap().interrupted = true;
        assert!(h.arm_working(), "taking the 已中断 note down draws a frame");
        assert!(!h.moment.read().unwrap().interrupted, "note spent");
        assert!(
            h.moment.read().unwrap().pending_working,
            "and the turn is armed"
        );
        assert!(
            !h.arm_working(),
            "arming with no note to take down is invisible"
        );
    }

    /// A turn that ends before any fact (an immediate error) must not leave the
    /// arm set to fire on the next turn's opening line.
    #[test]
    fn an_explicit_activity_spends_a_pending_arm() {
        use crate::moment::Activity;
        let h = host();
        h.arm_working();
        assert!(h.moment.read().unwrap().pending_working);
        h.set_activity(Activity::Idle); // a TurnComplete/Error before any fact
        assert!(
            !h.moment.read().unwrap().pending_working,
            "no arm left to fire on the next turn"
        );
        assert!(!h.settle_working(), "nothing armed to settle");
        assert_eq!(h.moment.read().unwrap().activity, Activity::Idle);
    }

    /// A session's name is read off the log, and the newest one wins — the
    /// first-prompt guess, then a model's summary, then whatever somebody
    /// typed. Nothing else keeps a copy of it.
    #[test]
    fn the_newest_name_is_the_sessions_name() {
        let h = host();
        assert_eq!(h.moment.read().unwrap().title, None, "unnamed to start");
        h.absorb(&SessionEvent::Titled {
            turn: 1,
            title: "修解析器".into(),
            user_set: false,
        });
        assert_eq!(h.moment.read().unwrap().title.as_deref(), Some("修解析器"));
        assert!(
            !h.moment.read().unwrap().title_user_set,
            "an auto title is not user-set"
        );
        h.absorb(&SessionEvent::Titled {
            turn: 2,
            title: "重构配置".into(),
            user_set: true,
        });
        assert_eq!(
            h.moment.read().unwrap().title.as_deref(),
            Some("重构配置"),
            "the newest wins"
        );
        assert!(
            h.moment.read().unwrap().title_user_set,
            "a /rename is user-set"
        );
    }

    /// A turn the person **cancelled** stays on the screen as one dim line per
    /// block: they stopped it halfway, and what it got done before they stopped
    /// is still worth reading. The model no longer sees it, which is what the
    /// dimming says (`docs/adr/0024` §17).
    ///
    /// A turn a *rewind* took back is a different thing and is drawn
    /// differently — see
    /// [`a_rewound_turn_is_not_drawn_at_all`](Self::a_rewound_turn_is_not_drawn_at_all).
    #[test]
    fn a_cancelled_turn_is_drawn_as_one_dim_line_each() {
        let h = host();
        two_turns(&h);
        let size = (60u16, 24u16);
        let before = h.compose(size);
        let shown = |frame: &Frame| {
            frame
                .part("stream")
                .map(|part| {
                    part.lines
                        .iter()
                        .map(|line| line.plain())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        };
        let whole = shown(&before);
        assert!(
            whole.contains("answer line one") && whole.contains("answer line three"),
            "the answer is drawn whole while the turn stands:\n{whole}"
        );

        assert!(h.mark_undone(
            std::collections::BTreeSet::from([2]),
            std::collections::BTreeSet::new(),
        ));
        let after = h.compose(size);
        let text = shown(&after);
        assert!(
            text.contains("the first thing"),
            "the turn that stands is untouched:\n{text}"
        );
        assert!(
            !text.contains("answer line two") && !text.contains("answer line three"),
            "the taken-back turn is one line, not its whole answer:\n{text}"
        );
        assert!(
            text.contains("answer line one"),
            "and that line is its own words:\n{text}"
        );
        let part = after.part("stream").expect("the stream is drawn");
        let dim = crate::theme::fg(crate::theme::Role::Muted);
        let taken_back: Vec<&Line> = part
            .lines
            .iter()
            .filter(|line| line.plain().contains("the second thing"))
            .collect();
        assert!(!taken_back.is_empty(), "its prompt is still there");
        for line in taken_back {
            assert!(
                line.spans.iter().all(|span| span.style.fg == dim.fg),
                "drawn dim: {:?}",
                line.spans
            );
        }
        // Nothing changed: no second frame is owed.
        assert!(!h.mark_undone(
            std::collections::BTreeSet::from([2]),
            std::collections::BTreeSet::new(),
        ));
    }

    /// **The two kinds of "taken back" are counted apart.**
    ///
    /// The screen asks the log two questions — which turns are gone from what
    /// the model sees, and which of those the person *rewound past* — and
    /// draws the answers differently. One set for both would make a cancelled
    /// turn vanish along with a rewound one, and what a cancelled turn got
    /// done before it was stopped is exactly what a person looks at next.
    #[test]
    fn a_cancelled_turn_counts_as_undone_but_not_as_rewound() {
        use atomcode_kernel::session::{rewound_turns, undone_turns};
        let said = |turn: u64| SessionEvent::UserMessage {
            turn,
            text: format!("turn {turn}"),
            images: Vec::new(),
        };
        let facts = [
            SessionEvent::TurnStart { turn: 1 },
            said(1),
            SessionEvent::TurnStart { turn: 2 },
            said(2),
            // Stopped by hand, and asked to be left out of what the model sees.
            SessionEvent::Interrupted {
                turn: 2,
                undone: true,
            },
            SessionEvent::TurnStart { turn: 3 },
            said(3),
            // A rewind back to the start of turn 3 — seq 6, counting from one.
            SessionEvent::Rewound {
                turn: 3,
                to: 6,
                scope: atomcode_harness::session::RewindScope::Conversation,
            },
        ];
        let logged: Vec<_> = facts
            .iter()
            .enumerate()
            .map(|(i, fact)| crate::conformance::logged(i, fact))
            .collect();
        let undone = undone_turns(&logged);
        let rewound = rewound_turns(&logged);
        assert!(
            undone.contains(&2) && undone.contains(&3),
            "both are gone from what the model sees: {undone:?}"
        );
        assert_eq!(
            rewound,
            std::collections::BTreeSet::from([3]),
            "but only turn 3 was rewound past — a cancel is not a rewind: \
             {rewound:?}"
        );
    }

    /// **A turn a rewind took back is not drawn at all.**
    ///
    /// Going back is not the same gesture as stopping: a person who rewinds
    /// says that turn should not have happened, and a screen still showing it
    /// is a conversation whose visible half disagrees with what the model is
    /// being sent. The `Rewound` block stays, so the history says what
    /// happened rather than quietly losing a stretch of itself.
    #[test]
    fn a_rewound_turn_is_not_drawn_at_all() {
        let h = host();
        two_turns(&h);
        let size = (60u16, 24u16);
        let shown = |h: &Host| {
            h.compose(size)
                .part("stream")
                .map(|part| {
                    part.lines
                        .iter()
                        .map(|line| line.plain())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        };
        assert!(shown(&h).contains("the second thing"), "it is there first");

        assert!(h.mark_undone(
            std::collections::BTreeSet::from([2]),
            std::collections::BTreeSet::from([2]),
        ));
        let text = shown(&h);
        assert!(
            !text.contains("the second thing") && !text.contains("answer line one"),
            "nothing of the rewound turn is left on screen:\n{text}"
        );
        assert!(
            text.contains("the first thing"),
            "and the turn that stands is untouched:\n{text}"
        );
    }

    /// A block whose row count follows the terminal's capabilities.
    ///
    /// A real one, not a mock: the judgement below is about whether the row
    /// index re-measures when the terminal changes, and a block that drew the
    /// same either way would let a broken index pass.
    #[derive(Debug)]
    struct CapsSized;

    impl crate::block::Content for CapsSized {
        fn kind(&self) -> &'static str {
            "caps_sized"
        }
        fn content_hash(&self) -> crate::block::ContentHash {
            crate::block::hash_of(&["caps_sized"])
        }
        fn lines(&self, ctx: &crate::block::RenderCtx) -> Vec<Line> {
            let n = if ctx.caps.unicode { 3 } else { 1 };
            (0..n).map(|_| Line::raw("sized")).collect()
        }
    }

    #[test]
    fn the_row_index_is_keyed_on_capability_and_not_only_width() {
        // A row count is what the scroll bound is computed from. If the cache
        // key carried only the width, a block that changed height when the
        // terminal changed would keep reporting the old count while the painter
        // drew the new one — a scroll limit that disagrees with the picture.
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        let h = Host::new(mods, default_layout());
        h.stream
            .write()
            .unwrap()
            .writer("test")
            .emit(crate::block::Coord::default(), Arc::new(CapsSized));

        let size = (80u16, 24u16);
        let unicode = h.stream_height(size, &h.moment.read().unwrap().clone());
        h.moment.write().unwrap().caps.unicode = false;
        let ascii = h.stream_height(size, &h.moment.read().unwrap().clone());

        assert_eq!(
            (unicode, ascii),
            (3, 1),
            "after the terminal changed, the count must be re-measured — keying \
             on width alone leaves the second number at 3"
        );
    }

    /// A bitmap payload: every cell default-coloured, row 0 in `first`, the rest
    /// in `rest`.
    fn raster_payload(columns: u16, rows: u16, first: char, rest: char) -> String {
        use base64::Engine as _;
        let mut bytes = Vec::new();
        for row in 0..rows {
            for _ in 0..columns {
                let ch = if row == 0 { first } else { rest };
                bytes.extend_from_slice(&(ch as u32).to_le_bytes());
                bytes.extend_from_slice(&0x0100_0000u32.to_le_bytes());
                bytes.extend_from_slice(&0x0100_0000u32.to_le_bytes());
            }
        }
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// A host whose whole screen is one bitmap pane.
    fn host_with_raster(columns: u16, rows: u16) -> Host {
        let mods = Arc::new(Modules::new());
        mods.add_view(Arc::new(
            Mounted::<crate::modules::raster::RasterPane>::new(),
        ))
        .unwrap();
        let h = Host::new(mods, Region::view(crate::modules::raster::ID));
        h.rasters
            .mount(
                crate::modules::raster::ID,
                crate::modules::raster::KEY,
                crate::raster::Raster::decode(
                    columns,
                    rows,
                    &raster_payload(columns, rows, '\u{2588}', '\u{2588}'),
                )
                .expect("a valid payload"),
            )
            .expect("mount");
        h
    }

    #[test]
    fn an_unchanged_bitmap_encodes_no_row_and_a_write_encodes_only_its_own() {
        // `ROWS_ENCODED` counts the rows a frame had to *encode*, which equality
        // of the picture cannot show: re-encoding an unchanged row emits the same
        // bytes. So this is the only way to assert "only what moved is redrawn".
        use std::sync::atomic::Ordering;
        let h = host_with_raster(6, 3);
        let size = (8u16, 4u16);
        let caps = crate::caps::Caps::default();

        // A first paint draws everything, and is the baseline.
        let a = crate::ansi::Lines::of(&h.compose(size), caps, None);
        let after_first = crate::ansi::ROWS_ENCODED.load(Ordering::Relaxed);

        // Nothing moved: a frame composes the same picture, and encodes no row.
        let b = crate::ansi::Lines::of(&h.compose(size), caps, Some(&a));
        assert_eq!(
            crate::ansi::ROWS_ENCODED.load(Ordering::Relaxed),
            after_first,
            "an unchanged bitmap must not re-encode a single row"
        );
        assert!(
            b.patch_from(Some(&a)).is_empty(),
            "and there is no patch to send"
        );

        // One row of the bitmap changes: exactly one screen row is re-encoded.
        h.rasters
            .write(
                crate::modules::raster::ID,
                crate::modules::raster::KEY,
                &raster_payload(6, 3, '\u{2580}', '\u{2588}'),
            )
            .expect("a write of the mounted size");
        let c = crate::ansi::Lines::of(&h.compose(size), caps, Some(&b));
        assert_eq!(
            crate::ansi::ROWS_ENCODED.load(Ordering::Relaxed) - after_first,
            1,
            "only the bitmap row that changed may be re-encoded"
        );
        assert!(
            !c.patch_from(Some(&b)).is_empty(),
            "and that row is what gets sent"
        );
    }

    #[test]
    fn a_bitmap_that_changes_nothing_needs_no_repaint_at_all() {
        // The other half of the claim: writing the *same* cells is not a change,
        // because the comparison is on the laid-out lines rather than on the
        // table's revision. Without this, a widget that repaints itself at 60Hz
        // with an identical frame would push the whole pane every time.
        use std::sync::atomic::Ordering;
        let h = host_with_raster(4, 2);
        let size = (6u16, 3u16);
        let caps = crate::caps::Caps::default();
        let a = crate::ansi::Lines::of(&h.compose(size), caps, None);
        let before = crate::ansi::ROWS_ENCODED.load(Ordering::Relaxed);
        h.rasters
            .write(
                crate::modules::raster::ID,
                crate::modules::raster::KEY,
                &raster_payload(4, 2, '\u{2588}', '\u{2588}'),
            )
            .expect("a write of the mounted size");
        let b = crate::ansi::Lines::of(&h.compose(size), caps, Some(&a));
        assert_eq!(
            crate::ansi::ROWS_ENCODED.load(Ordering::Relaxed),
            before,
            "the same cells must not cost a repaint"
        );
        assert!(b.patch_from(Some(&a)).is_empty());
    }

    /// A producer that opens with one line naming where we are.
    ///
    /// Counts its own asks, so a judgement can tell "the second producer was never
    /// consulted" from "it was consulted and said nothing".
    struct Opener {
        said: std::sync::atomic::AtomicU32,
    }

    impl crate::module::Producer for Opener {
        fn id(&self) -> &'static str {
            "opener"
        }
        fn absorb(
            &self,
            _logged: &atomcode_harness::session::LoggedEvent,
            _out: &mut crate::block::StreamWriter<'_>,
        ) {
        }
        fn opening(
            &self,
            _at: crate::block::Coord,
            open: &crate::module::Opening,
        ) -> Option<Arc<dyn crate::block::Content>> {
            self.said.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(Arc::new(crate::content::NoticeBlock {
                detail: format!("hello from {}", open.cwd),
            }))
        }
    }

    #[test]
    fn the_first_thing_a_conversation_says_happens_once_and_only_when_it_is_empty() {
        let mods = Arc::new(Modules::new());
        let opener = Arc::new(Opener {
            said: std::sync::atomic::AtomicU32::new(0),
        });
        mods.add_producer(opener.clone()).unwrap();
        let h = Host::new(mods, default_layout());

        let open = crate::module::Opening {
            cwd: "~/proj".into(),
            ..Default::default()
        };
        assert!(
            h.open_conversation(crate::block::Coord::default(), &open),
            "an empty stream must be opened"
        );
        assert_eq!(h.stream.read().unwrap().len(), 1);

        assert!(
            !h.open_conversation(crate::block::Coord::default(), &open),
            "a stream that already has something in it is not opening"
        );
        assert_eq!(h.stream.read().unwrap().len(), 1, "still just the one");
        assert_eq!(
            opener.said.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the producer must not be asked twice — the stream was not empty, so \
             the loop should not have turned at all"
        );
    }

    #[test]
    fn the_first_producer_with_something_to_say_wins_and_the_rest_are_not_asked() {
        // The rule is "when the stream is empty", and the first answer makes it
        // not-empty. So no second mechanism is needed to stop a helper opener, and
        // the count proves the second one was never consulted.
        struct Silent;
        impl crate::module::Producer for Silent {
            fn id(&self) -> &'static str {
                "silent"
            }
            fn absorb(
                &self,
                _logged: &atomcode_harness::session::LoggedEvent,
                _out: &mut crate::block::StreamWriter<'_>,
            ) {
            }
            // `opening` keeps its default: `None`.
        }
        struct Extra {
            said: std::sync::atomic::AtomicU32,
        }
        impl crate::module::Producer for Extra {
            fn id(&self) -> &'static str {
                "extra"
            }
            fn absorb(
                &self,
                _logged: &atomcode_harness::session::LoggedEvent,
                _out: &mut crate::block::StreamWriter<'_>,
            ) {
            }
            fn opening(
                &self,
                _at: crate::block::Coord,
                _open: &crate::module::Opening,
            ) -> Option<Arc<dyn crate::block::Content>> {
                self.said.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(Arc::new(crate::content::NoticeBlock {
                    detail: "second".into(),
                }))
            }
        }
        let mods = Arc::new(Modules::new());
        mods.add_producer(Arc::new(Silent)).unwrap();
        let silent_first = Arc::new(Opener {
            said: std::sync::atomic::AtomicU32::new(0),
        });
        mods.add_producer(silent_first.clone()).unwrap();
        let never = Arc::new(Extra {
            said: std::sync::atomic::AtomicU32::new(0),
        });
        mods.add_producer(never.clone()).unwrap();
        let h = Host::new(mods, default_layout());

        assert!(h.open_conversation(crate::block::Coord::default(), &Default::default()));
        assert_eq!(
            silent_first.said.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the first producer that can answer, did"
        );
        assert_eq!(
            never.said.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the third producer was asked even though the stream was already open"
        );
    }

    #[test]
    fn an_opening_block_lands_settled_because_it_is_not_still_growing() {
        let mods = Arc::new(Modules::new());
        mods.add_producer(Arc::new(Opener {
            said: std::sync::atomic::AtomicU32::new(0),
        }))
        .unwrap();
        let h = Host::new(mods, default_layout());
        h.open_conversation(crate::block::Coord::default(), &Default::default());
        let stream = h.stream.read().unwrap();
        assert!(
            stream.slots()[0].is_settled(),
            "an opening has no stage at which it is still growing"
        );
    }

    /// A tail module whose height follows `Moment::activity` and nothing else.
    ///
    /// The live line is the module this is really about, but it does not answer
    /// the question yet: `live::showing` still returns nothing while the reader
    /// is scrolled back, so its height does not move and a judgement about the
    /// activity route would pass against machinery that never ran. This is the
    /// instrument for the mechanism, and it keeps meaning something after the
    /// live line joins in (ADR 0020 Step 4).
    pub struct Peek;

    impl crate::module::View for Peek {
        type State = ();
        fn id() -> &'static str {
            "peek"
        }
        fn absorb(_: &mut (), _: &SessionEvent) {}
        fn render(_: &(), vp: &crate::moment::Viewport<'_>) -> Vec<Line> {
            (0..vp.rect.h).map(|_| Line::raw("peek")).collect()
        }
        fn height(_: &(), moment: &Moment, _: u16) -> Height {
            match moment.activity {
                crate::moment::Activity::Idle => Height::Hug(0),
                _ => Height::Hug(PEER_ROWS),
            }
        }
    }

    const PEER_ROWS: u16 = 2;

    /// A question takes the field's rows without touching what is in it.
    ///
    /// The composer steps aside while a question is up (`asked_height`), and the
    /// half of that which is easy to get wrong is the other half: hiding a box is
    /// not clearing it. A person who had typed half a sentence and was interrupted
    /// by a question must find it still there afterwards.
    #[test]
    fn a_question_hides_the_field_without_losing_what_was_typed() {
        let h = host();
        {
            let mut m = h.moment.write().unwrap();
            m.input = "half a sentence".into();
            m.caret = m.input.len();
        }
        // No panel row in this tree, so nothing is expected to step aside.
        h.asks.push(atomcode_harness::seams::Question::plain(
            "Allow?",
            &["yes", "no"],
        ));
        h.sync_asking();
        assert!(
            h.moment.read().unwrap().asking.is_none(),
            "with no panel mounted the question belongs to the stream, not the moment"
        );
        assert_eq!(
            h.moment.read().unwrap().input,
            "half a sentence",
            "and the draft is where it was"
        );

        // With the row mounted, the question reaches the moment — and the draft
        // still survives it.
        h.modules
            .add_view(Arc::new(Mounted::<crate::modules::ask::Ask>::new()))
            .unwrap();
        assert!(h.sync_asking(), "the question arrives on the moment");
        assert!(h.moment.read().unwrap().asking.is_some());
        assert_eq!(
            h.moment.read().unwrap().input,
            "half a sentence",
            "stepping aside is not clearing"
        );

        // Answer it: the panel goes, and the draft is handed back with it.
        h.asks.take().unwrap().answer(Some("yes".into()));
        assert!(h.sync_asking(), "the question leaving is a change too");
        assert!(h.moment.read().unwrap().asking.is_none());
        assert_eq!(
            h.moment.read().unwrap().input,
            "half a sentence",
            "and it is handed back, not lost"
        );
    }

    /// The settings panel stands where the composer does, and gives it back.
    ///
    /// The same bargain a question strikes, and checked here for the same
    /// reason: `asked_height` is the one place that arbitrates, and a panel that
    /// is drawn over a composer without the arbitration knowing about it is a
    /// screen with two things on one row — which is what the settings panel
    /// would be if this were a `Show` op instead.
    ///
    /// Measured as a *displacement* rather than as a row count: what the field
    /// asks for is its own business and may change; what has to hold is that
    /// while the panel is up it asks for none, and afterwards for what it did
    /// before.
    #[test]
    fn the_settings_panel_takes_the_composers_rows_and_hands_them_back() {
        let h = host();
        let size = (60u16, 30u16);
        let w = size.0;

        let before = {
            let mods = h.modules.clone();
            let m = h.moment.read().unwrap().clone();
            crate::host::asked_height(&mods, crate::modules::input::ID, &m, w)
        };
        assert!(before > 0, "the field has rows to give");

        assert!(!h.settings_open(), "nothing is up to begin with");
        assert!(h.toggle_settings(), "and it opens");
        assert!(h.settings_open());

        let displaced = {
            let mods = h.modules.clone();
            let m = h.moment.read().unwrap().clone();
            crate::host::asked_height(&mods, crate::modules::input::ID, &m, w)
        };
        assert_eq!(displaced, 0, "the composer stood aside");
        assert!(
            displaces_composer(&h.moment.read().unwrap()),
            "and the arbitration agrees that something stands there"
        );

        // The tip row goes with it: the composer is a whole, and a blank row
        // left above a panel is the shadow of a box that is not there.
        let tip = {
            let mods = h.modules.clone();
            let m = h.moment.read().unwrap().clone();
            crate::host::asked_height(&mods, crate::modules::tip::ID, &m, w)
        };
        assert_eq!(tip, 0, "the reserved row went too");

        assert!(h.close_settings(), "put it away");
        let after = {
            let mods = h.modules.clone();
            let m = h.moment.read().unwrap().clone();
            crate::host::asked_height(&mods, crate::modules::input::ID, &m, w)
        };
        assert_eq!(after, before, "and the field is exactly as it was");
        assert!(!displaces_composer(&h.moment.read().unwrap()));
    }

    /// A password is asked on the composer's line, so nothing may take the line
    /// away while one is being asked — not even the two panels that otherwise
    /// stand there.
    ///
    /// The case is real rather than theoretical: two tools run in one batch, one
    /// asks for approval and the other hits a `sudo`. If the question kept the
    /// composer's rows the prompt would be off the screen while the keys still
    /// went to it — a person typing a password into nothing, with both the turn
    /// and the `sudo` waiting on them.
    #[tokio::test]
    async fn a_password_keeps_the_line_a_question_would_have_taken() {
        let h = host();
        h.modules
            .add_view(Arc::new(Mounted::<crate::modules::ask::Ask>::new()))
            .unwrap();
        let w = 60u16;
        let rows = |h: &Host| {
            let mods = h.modules.clone();
            let m = h.moment.read().unwrap().clone();
            crate::host::asked_height(&mods, crate::modules::input::ID, &m, w)
        };
        let before = rows(&h);
        assert!(before > 0, "the field has rows to give");

        h.asks.push(atomcode_harness::seams::Question::plain(
            "Allow?",
            &["yes", "no"],
        ));
        assert!(h.sync_asking());
        assert_eq!(rows(&h), 0, "the question stands where the composer does");

        let (reply, answered) = tokio::sync::oneshot::channel();
        h.ask_secret("[sudo] password for lichao:", reply);
        assert_eq!(rows(&h), before, "and gives the line back for the password");
        assert!(!displaces_composer(&h.moment.read().unwrap()));
        assert!(h.secret_waiting(), "which is where the keys are going");

        // Answered, the field is the question's business again.
        assert!(!h.secret_key(crate::surface::KeyPress::ch('p')));
        assert!(h.secret_key(crate::surface::KeyPress::plain(crate::surface::Key::Enter)));
        assert_eq!(answered.await.unwrap().as_deref(), Some("p"));
        assert_eq!(rows(&h), 0, "the question has the line back");
    }

    /// Shutdown refuses a password still being asked for, and says so through
    /// the same channel `sudo` is blocked on. Fail-closed: a refusal is `None`,
    /// never a blank password.
    #[tokio::test]
    async fn a_screen_going_away_refuses_the_password_it_was_asked_for() {
        let h = host();
        let (reply, answered) = tokio::sync::oneshot::channel();
        h.ask_secret("password:", reply);
        assert!(h.moment.read().unwrap().secret.is_some(), "on screen");
        h.refuse_secret();
        assert_eq!(answered.await.unwrap(), None);
        assert!(h.moment.read().unwrap().secret.is_none(), "and off it");
    }

    /// Opening the panel twice does not throw away what was typed in it.
    ///
    /// A toggle that rebuilt the panel each time would clear a search the person
    /// is in the middle of — `/config` typed twice being an easy accident, since
    /// the second one is what a person does when they are not sure it worked.
    #[test]
    fn asking_for_the_panel_again_does_not_clear_what_is_in_it() {
        let h = host();
        assert!(h.toggle_settings());
        {
            let mut m = h.moment.write().unwrap();
            let panel = m.settings_panel.as_mut().unwrap();
            panel.type_into_search('主');
        }
        assert!(h.settings_open());

        // The second `/config` closes it — a toggle is a toggle.
        assert!(h.toggle_settings());
        assert!(!h.settings_open(), "the second ask puts it away");
    }

    /// Closing the panel is what Escape does, and it changes nothing else.
    #[test]
    fn closing_the_panel_leaves_no_trace_in_the_composer() {
        let h = host();
        {
            let mut m = h.moment.write().unwrap();
            m.input = "half a sentence".into();
            m.caret = m.input.len();
        }
        h.toggle_settings();
        assert!(h.settings_open());

        let (changed, set) =
            h.settings_key(crate::surface::KeyPress::plain(crate::surface::Key::Esc));
        assert!(changed, "a key that closed it is a change to the screen");
        assert!(set.is_none(), "and not a setting to write");
        assert!(!h.settings_open(), "it is down");
        assert_eq!(
            h.moment.read().unwrap().input,
            "half a sentence",
            "standing aside is not clearing — the draft is where it was"
        );
    }

    /// A host with the settings panel mounted, and two settings in it.
    fn host_with_settings() -> Host {
        use crate::settings::{Applies, SettingKind, SettingRow, SettingsView};

        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(
            Mounted::<crate::modules::settings::Settings>::new(),
        ))
        .unwrap();
        mods.add_view(Arc::new(Mounted::<input::Input>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        let h = Host::new(mods, default_layout());
        {
            let row = |id: &str, label: &str, value: &str| SettingRow {
                id: id.into(),
                label: label.into(),
                value: value.into(),
                kind: SettingKind::Boolean,
                applies: Applies::Reload,
            };
            h.moment.write().unwrap().settings = SettingsView::new(vec![
                row("a.first", "第一", "true"),
                row("b.second", "第二", "true"),
            ]);
        }
        h
    }

    /// A press on the inner row of tabs shows the page under the pointer, and
    /// a press anywhere else on the panel does not.
    ///
    /// Read off the same layout the frame drew with, which is the rule every
    /// hit test on this panel keeps: the inner row moves with the page's
    /// chrome, so a test that assumed a fixed screen row would answer for
    /// whatever happened to be drawn there instead.
    #[test]
    fn a_press_on_the_inner_tabs_shows_the_page_under_the_pointer() {
        let h = host_with_settings();
        h.toggle_settings();
        {
            let mut m = h.moment.write().expect("moment poisoned");
            m.settings_panel
                .as_mut()
                .expect("a panel")
                .show(crate::settings::Tab::Stats);
        }
        let frame = h.compose((60, 30));
        let part = frame
            .part(crate::modules::settings::ID)
            .expect("the panel is drawn");
        let rect = part.rect;
        let drawn: Vec<String> = part.lines.iter().map(|l| l.plain()).collect();
        let row = drawn
            .iter()
            .position(|line| line.contains("Models"))
            .expect("the inner row is drawn");

        // Every cell of a label answers with that label's page, and the gaps
        // between them answer with nothing.
        let at = drawn[row].find("Models").expect("just found it");
        assert_eq!(
            h.settings_stats_page_at(rect.x + at as u16, rect.y + row as u16),
            Some(crate::settings::StatsPage::Models),
            "the cell under the pointer: {:?}",
            drawn[row]
        );
        assert!(
            h.settings_stats_page_at(rect.x, rect.y + row as u16)
                .is_none(),
            "and the indent before them is not a tab: {:?}",
            drawn[row]
        );
        // A row that is not the inner row is not the inner row, whatever column
        // the pointer is in.
        assert!(
            h.settings_stats_page_at(rect.x + at as u16, rect.y + row as u16 + 1)
                .is_none(),
            "the row below it is content"
        );

        assert!(h.show_stats_page(crate::settings::StatsPage::Models));
        assert_eq!(
            h.moment
                .read()
                .expect("moment poisoned")
                .settings_panel
                .as_ref()
                .expect("a panel")
                .stats,
            crate::settings::StatsPage::Models
        );
    }

    /// The wheel over a page that is read scrolls that page, and the wheel
    /// anywhere else is still the conversation's.
    ///
    /// Both halves matter. The panel's read-only pages can be longer than it
    /// is, so a wheel over one that scrolled the transcript *behind* it would
    /// be a wheel that does nothing a person can see — and a panel that
    /// answered every wheel event while it happened to be open would take the
    /// wheel away from the transcript it does not even cover.
    #[test]
    fn the_wheel_over_a_read_page_scrolls_it_and_elsewhere_does_not() {
        let h = host_with_settings();
        h.toggle_settings();
        {
            let mut m = h.moment.write().expect("moment poisoned");
            let panel = m.settings_panel.as_mut().expect("a panel");
            panel.show(crate::settings::Tab::Usage);
        }
        let frame = h.compose((60, 30));
        let rect = frame
            .part(crate::modules::settings::ID)
            .expect("the panel is drawn")
            .rect;

        assert!(
            h.settings_wheel(rect.x + 1, rect.y + 1, 3),
            "over the panel, the wheel is the panel's"
        );
        let after = h
            .moment
            .read()
            .expect("moment poisoned")
            .settings_panel
            .as_ref()
            .expect("a panel")
            .scroll;
        assert_eq!(after, 3, "and it moved the page");

        // Above the panel is the conversation, which is the thing people scroll
        // all day.
        assert!(
            !h.settings_wheel(rect.x + 1, rect.y.saturating_sub(1), 3),
            "off the panel, the wheel is not the panel's"
        );

        // The settings page is narrowed by typing, not scrolled: a list you can
        // both filter and scroll has two ways to lose the row you were on.
        {
            let mut m = h.moment.write().expect("moment poisoned");
            let panel = m.settings_panel.as_mut().expect("a panel");
            panel.show(crate::settings::Tab::Config);
        }
        assert!(
            !h.settings_wheel(rect.x + 1, rect.y + 1, 3),
            "the page that is filtered does not scroll"
        );
    }

    /// A click reads the row the frame drew: the pointed row is the one the
    /// pointer is over, and pressing takes it.
    ///
    /// The property is the question panel's, checked the same way: the row a
    /// press lands on is decided by the rect the panel was **drawn** in, so a
    /// click and the highlight cannot come from two arrangements of one list.
    #[test]
    fn a_click_on_a_setting_reads_the_row_the_frame_drew() {
        let h = host_with_settings();
        let size = (60u16, 30u16);
        h.toggle_settings();

        let frame = h.compose(size);
        let part = frame
            .part(crate::modules::settings::ID)
            .expect("the panel is drawn");
        let rect = part.rect;
        let drawn: Vec<String> = part.lines.iter().map(|l| l.plain()).collect();

        // Every drawn row that holds a setting answers with the index of the
        // setting it draws, and no row holds two.
        let mut seen: Vec<usize> = Vec::new();
        for (row, text) in drawn.iter().enumerate() {
            if let Some(i) = h.settings_row_at(rect.x + 1, rect.y + row as u16) {
                assert!(!seen.contains(&i), "row {row} answers {i}, already seen");
                seen.push(i);
                let wanted = if i == 0 { "第一" } else { "第二" };
                assert!(
                    text.contains(wanted),
                    "row {row} answers setting {i} but draws {text:?}"
                );
            }
        }
        assert_eq!(
            seen.len(),
            2,
            "both settings are reachable by click:\n{}",
            drawn.join("\n")
        );

        // And a press on one arms it.
        let row_of_second = drawn
            .iter()
            .position(|t| t.contains("第二"))
            .expect("the second row is drawn");
        assert_eq!(
            h.settings_row_at(rect.x + 1, rect.y + row_of_second as u16),
            Some(1)
        );
        assert!(h.point_settings_at(1), "the press moved the highlight");
        assert_eq!(
            h.moment
                .read()
                .unwrap()
                .settings_panel
                .as_ref()
                .unwrap()
                .cursor,
            1
        );
    }

    /// Nothing is drawn, nothing is clickable.
    ///
    /// The negative control for the criterion above. The interesting half is the
    /// *stale* rect: closing the panel leaves the last frame's rect in `hits`
    /// until the next `compose`, and a pointer press can arrive in between —
    /// which is exactly why `settings_row_at` asks whether a panel is up before
    /// it asks what row a point is on. Composing after the close would paper
    /// over that (the frame rebuilds `hits`), so this deliberately does not.
    #[test]
    fn a_click_missing_the_panel_hits_nothing() {
        let h = host_with_settings();
        let size = (60u16, 30u16);

        // Never opened: no rect has ever been recorded.
        assert_eq!(h.settings_row_at(1, 1), None);
        assert_eq!(h.settings_row_at(30, 15), None);

        // Opened and drawn: the point now lands on a setting. The row is read
        // off what was drawn rather than assumed from an offset — the panel's
        // own layout decides how many rows the search box takes, and a constant
        // here would be a second copy of that decision.
        h.toggle_settings();
        let frame = h.compose(size);
        let part = frame
            .part(crate::modules::settings::ID)
            .expect("the panel is drawn");
        let rect = part.rect;
        let first = part
            .lines
            .iter()
            .position(|l| l.plain().contains("第一"))
            .expect("the first setting is drawn");
        let on_a_setting = (rect.x + 1, rect.y + first as u16);
        assert!(
            h.settings_row_at(on_a_setting.0, on_a_setting.1).is_some(),
            "the point is on a setting while the panel is up"
        );

        // Closed, and **not composed**: the rect is still in `hits` and the
        // place it was drawn must not answer.
        assert!(h.close_settings());
        assert_eq!(
            h.settings_row_at(on_a_setting.0, on_a_setting.1),
            None,
            "the place it used to be is not a place it is"
        );

        // A compose does clear the rect — the other half of the same story, and
        // the reason the check above cannot be replaced by "compose first".
        assert!(h.compose(size).part(crate::modules::settings::ID).is_none());
        assert_eq!(h.settings_row_at(on_a_setting.0, on_a_setting.1), None);

        // And a point on the panel but not on a setting — the top margin above
        // the search box — is not a row either.
        h.toggle_settings();
        let frame = h.compose(size);
        let rect = frame.part(crate::modules::settings::ID).unwrap().rect;
        assert_eq!(
            h.settings_row_at(rect.x + 1, rect.y),
            None,
            "the top margin holds no setting"
        );
    }

    /// A press switches pages only on the header row.
    ///
    /// The tabs are drawn on the panel's first row and nowhere else, so a press
    /// lower down must not switch pages because the cell happens to line up with
    /// a tab's column. Checked at the column that *does* hold a tab, one row
    /// down — the point of the criterion is that the row is the answer, not the
    /// column.
    #[test]
    fn a_press_switches_pages_only_on_the_header_row() {
        let h = host_with_settings();
        let size = (60u16, 30u16);
        h.toggle_settings();
        let frame = h.compose(size);
        let part = frame
            .part(crate::modules::settings::ID)
            .expect("the panel is drawn");
        let rect = part.rect;

        // A column that holds a tab: found from the drawn row, so the test does
        // not carry its own copy of where the tabs are.
        let header = part.lines.first().expect("the header is drawn").plain();
        let col = header.find("Config").expect("the Config tab is on the row") + 1;

        assert_eq!(
            h.settings_tab_at(rect.x + col as u16, rect.y),
            Some(crate::settings::Tab::Config),
            "on the header row it is that tab"
        );

        // Every other row of the panel: the same column, and it is not a tab.
        for row in 1..part.lines.len().min(6) {
            assert_eq!(
                h.settings_tab_at(rect.x + col as u16, rect.y + row as u16),
                None,
                "row {row} of the panel is not the tab row"
            );
        }

        // And off the panel entirely — below it — is not either.
        assert_eq!(
            h.settings_tab_at(rect.x + col as u16, rect.y + rect.h),
            None,
            "below the panel is not the panel"
        );
        assert_eq!(h.settings_tab_at(0, 0), None, "nor is anywhere else");
    }

    /// A press on a tab shows that page, through the same call the keyboard uses.
    #[test]
    fn a_press_on_a_tab_shows_the_page() {
        let h = host_with_settings();
        h.toggle_settings();
        assert_eq!(
            h.moment
                .read()
                .unwrap()
                .settings_panel
                .as_ref()
                .unwrap()
                .tab,
            crate::settings::Tab::Config,
            "it opens on the settings"
        );

        assert!(h.show_settings_tab(crate::settings::Tab::Usage));
        assert_eq!(
            h.moment
                .read()
                .unwrap()
                .settings_panel
                .as_ref()
                .unwrap()
                .tab,
            crate::settings::Tab::Usage
        );
        assert!(
            !h.show_settings_tab(crate::settings::Tab::Usage),
            "and asking for the page that is already showing is not a change"
        );

        // With no panel up there is nothing to show, and it says so rather than
        // opening one behind the caller's back.
        h.close_settings();
        assert!(!h.show_settings_tab(crate::settings::Tab::Stats));
        assert!(!h.settings_open(), "no panel was opened by asking");
    }

    /// A question drawn at the foot of the stream is counted in the scroll.
    ///
    /// The fallback path: with no `tui-panel-ask` row mounted the question is
    /// plain lines below the conversation (`stream_lines`), and a row the painter
    /// draws but `stream_height_in` does not count is a row out of reach at the
    /// bottom of the scroll — the last thing the question has to be is unreachable.
    ///
    /// Checked as a *delta*: the same host, the same size, the only difference
    /// being that a question is waiting. An absolute number here would be a
    /// second copy of the block's height, and it would keep passing if the
    /// question stopped being counted and the fixture changed.
    #[test]
    fn a_question_at_the_foot_of_the_stream_is_counted_in_the_scroll() {
        let h = host();
        let size = (80u16, 24u16);
        let before = h.stream_height(size, &h.moment.read().unwrap().clone());

        // `pub(crate)`, and dropped on purpose: this asks what a *waiting*
        // question does to the geometry, not who is waiting on the answer.
        drop(h.asks.push(atomcode_harness::seams::Question::plain(
            "Allow?",
            &["yes", "no"],
        )));
        h.sync_asking();
        let after = h.stream_height(size, &h.moment.read().unwrap().clone());

        assert!(
            after > before,
            "the question is drawn but not counted: {before} -> {after}, so its rows \
             are out of reach at the bottom of the scroll"
        );

        // And it is reachable: what the scroll can reach plus what the window
        // shows must cover every row there is. Written as `>=` and not `==`: a
        // conversation shorter than its pane has no scroll at all, so the limit
        // saturates at zero and the sum exceeds the content — which is correct,
        // and exactly the case an equality here got wrong.
        let limit = h.scroll_limit(size, &h.moment.read().unwrap().clone());
        let rows = h.stream_rows(size, &h.moment.read().unwrap().clone()) as usize;
        assert!(
            after <= limit + rows,
            "the scroll cannot reach the last row of the question: {after} rows of \
             content, a window of {rows} and a limit of {limit}"
        );
    }

    /// A host whose tail is that module, with enough in the conversation to
    /// scroll back through.
    fn host_with_peek_tail() -> Host {
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<Peek>::new())).unwrap();
        mods.add_view(Arc::new(Mounted::<crate::modules::status::Status>::new()))
            .unwrap();
        Host::new(
            mods,
            Region::split(
                crate::region::Dir::Vertical,
                crate::region::Constraint::Fill,
                Region::stream().with_tail(["peek"]),
                Region::split(
                    crate::region::Dir::Vertical,
                    crate::region::Constraint::Fill,
                    Region::view(crate::modules::input::ID),
                    Region::view(crate::modules::status::ID),
                ),
            ),
        )
    }

    /// A conversation long enough to scroll back from the bottom.
    fn fed_and_scrollable(h: &Host) {
        for i in 0..30 {
            h.absorb(&SessionEvent::AssistantMessage {
                turn: 1,
                round: i,
                text: format!("row {i}"),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            });
        }
    }

    fn fed() -> Host {
        let h = host();
        for f in conformance::facts() {
            h.absorb(&f);
        }
        h
    }

    /// Every tree this build ships, by name.
    fn shipped_trees() -> Vec<(String, Region)> {
        vec![("default".to_string(), default_layout())]
    }

    #[test]
    fn the_layout_this_build_ships_names_no_module_twice() {
        // `default_layout()` is a tree that never passes through
        // `Layout::apply`, which is where a tail id that is also a leaf gets
        // refused. A shipped tree that named one twice would draw it twice on
        // every frame, and nothing else in the suite would notice — `compose`
        // would simply place it in both regions.
        for (name, tree) in shipped_trees() {
            assert_eq!(
                tree.named_twice(),
                None,
                "the {name} layout names one twice"
            );
        }
    }

    #[test]
    fn every_shipped_tree_rides_the_tail_exactly_once() {
        // The other direction, and the one `named_twice` cannot see: a tree that
        // names a tail module **nowhere** is not naming it twice, so dropping
        // the tail from a shipped tree would leave that arrangement quietly missing
        // its task list while every other check stayed green.
        //
        // "Exactly once" rather than "at least once" because the two faults are
        // guarded in the same breath: too few is this, too many is
        // `named_twice`, and a module named twice is also named once.
        for (name, tree) in shipped_trees() {
            let tail = tree.tail().to_vec();
            for id in TAIL {
                let rides = tail.iter().filter(|t| t == id).count();
                let leaves = tree.modules().iter().filter(|m| m == id).count() - rides;
                assert_eq!(
                    leaves, 0,
                    "`{id}` is a leaf of its own in the {name} layout as well as a tail id"
                );
                assert_eq!(
                    rides, 1,
                    "`{id}` rides the tail {rides} times in the {name} layout — every \
                     shipped tree has to carry `TAIL` exactly once, or that arrangement \
                     silently loses it"
                );
            }
        }
    }

    #[test]
    fn a_planned_todo_list_scrolls_with_the_conversation() {
        // The point of the whole change, and the one thing an assertion on
        // `part("todo")` cannot tell: a panel that is *placed* is placed whether
        // it is pinned to the foot of the screen or riding the conversation.
        // What says which one it is, is whether scrolling takes it away.
        let h = host_with_real_todo();
        for i in 0..40 {
            h.absorb(&SessionEvent::AssistantMessage {
                turn: 1,
                round: i,
                text: format!("row {i}"),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            });
        }
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 40,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: "t1".into(),
                name: "todowrite".into(),
                arguments: serde_json::json!({
                    "todos": [ { "content": "only", "status": "in_progress" } ]
                })
                .to_string(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        });
        let size = (80, 24);

        let at_bottom = h.compose(size);
        let todo = at_bottom
            .part("todo")
            .expect("the plan is up while the reader is at the bottom");
        // Inside the conversation's region, and above the frame's first row —
        // where the tail belongs. Which exact row is the tail's business, not
        // this judgement's: what it has to establish is that the plan is part
        // of the scrolling content rather than a band of its own.
        let stream_rect = at_bottom.part("stream").expect("the words").rect;
        assert!(
            todo.rect.y + todo.rect.h <= size.1,
            "the plan runs off the bottom of the screen: {todo:?}"
        );
        assert!(
            todo.rect.y >= stream_rect.y,
            "the plan is above the conversation rather than at its tail"
        );

        // Scrolled far enough back and it is gone — not moved, gone. A pinned
        // panel would still have a rect here.
        let plan_rows = todo.rect.h as usize;
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(plan_rows);
        let scrolled = h.compose(size);
        assert!(
            scrolled.part("todo").is_none(),
            "the plan is still on screen after scrolling past it: it is pinned, \
             not riding the conversation"
        );

        // And what it gave back went to the conversation.
        assert_eq!(
            scrolled.part("stream").expect("the words").rect.h as usize,
            at_bottom.part("stream").expect("the words").rect.h as usize + plan_rows,
            "the rows the tail gave up are the conversation's again"
        );
    }

    /// The shipped default layout, with the task list really mounted.
    ///
    /// `host()` has neither, and a tail judgement written against it is vacuous:
    /// no tail id resolves, the geometry is the identity, and every assertion
    /// passes against machinery that never ran. This is the fixture the tail
    /// checks have to start from.
    fn host_with_real_todo() -> Host {
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<crate::modules::todo::Todo>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<input::Input>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        Host::new(mods, default_layout())
    }

    /// The shipped default layout with **everything `TAIL` names** mounted.
    ///
    /// Built from `TAIL` rather than listed by hand: a fixture that named the
    /// modules itself would keep passing after `TAIL` grew, while the thing it
    /// is supposed to exercise stopped being the shipped arrangement.
    fn host_with_the_shipped_tail() -> Host {
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<crate::modules::todo::Todo>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<crate::modules::live::Live>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<input::Input>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<crate::modules::tip::Tip>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        Host::new(mods, default_layout())
    }

    #[test]
    fn opening_a_call_leaves_the_row_that_was_clicked_where_it_was() {
        // **At the bottom**, which is the common case and the one the old code
        // skipped: it only pinned while the reader was scrolled back.
        //
        // A tool call arrives folded (`Presentation::default_folds`), so the
        // click *opens* it — and opening grows the block **upward**, which
        // pushes its own header off the top. The person pointed at that row; it
        // is the one that has to stay.
        let h = host_with_a_long_call();
        // Tool calls open by default now; fold so the click under test *opens* one.
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Each);
        let size = (80, 24);
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos::BOTTOM;
        let _ = h.compose(size);

        let frame = h.compose(size);
        let y_before = anchor_row(&frame, "ReadFile(anchor-line)");
        let (id, kind) = h
            .block_at(10, y_before)
            .expect("the call's header is a fold target");
        assert_eq!(
            h.moment.read().unwrap().scroll.0,
            0,
            "the reader has to be at the bottom for this to be the case it is about"
        );
        let folded_h = h.stream_height(size, &h.moment.read().unwrap().clone());

        // The click, held.
        h.held_while(|| {
            h.toggle_block(id, kind);
        });

        let after = h.compose(size);
        let opened_h = h.stream_height(size, &h.moment.read().unwrap().clone());
        assert!(
            opened_h > folded_h,
            "the click did not open it ({folded_h} rows before, {opened_h} after), so \
             there is nothing for the pin to hold"
        );
        assert_ne!(
            h.moment.read().unwrap().scroll.0,
            0,
            "nothing was pinned: the reading never moved, so the row had to"
        );
        assert_eq!(
            anchor_row(&after, "ReadFile(anchor-line)"),
            y_before,
            "the row that was clicked moved, so the pin did not hold it"
        );
    }

    /// The screen row the call's own header is drawn on.
    ///
    /// The header, not its result rows: the header is the row a click folds, so
    /// it is the row that has to stay put.
    fn anchor_row(frame: &Frame, needle: &str) -> u16 {
        let stream = frame.part("stream").expect("the conversation");
        for (i, line) in stream.lines.iter().enumerate() {
            if line.plain().contains(needle) {
                return stream.rect.y + i as u16;
            }
        }
        panic!(
            "no line containing {needle:?} on screen: {:#?}",
            stream.lines.iter().map(|l| l.plain()).collect::<Vec<_>>()
        )
    }

    /// A host whose conversation is short, with one foldable tool call in it.
    ///
    /// Short on purpose: the block that gets folded has to be on screen in the
    /// frame *before* the fold, or there is no row to hold in place and the
    /// judgement is about nothing.
    fn host_with_a_long_call() -> Host {
        let h = host();
        // Enough above it to scroll through: with a conversation that fits the
        // pane there is nowhere to scroll *to*, and the pin would be clamping
        // to a limit of zero — which is a correct answer and a pointless test.
        for i in 0..30 {
            h.absorb(&SessionEvent::AssistantMessage {
                turn: 1,
                round: i,
                text: format!("lead-in {i}"),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            });
        }
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 30,
            text: "before the call".into(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            reasoning_blocks: Vec::new(),
            meta: None,
        });
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 31,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({ "path": "anchor-line" }).to_string(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        });
        // Unfolded, the call draws several rows: the header, then its result.
        // Folding it back to one line is what moves everything below it.
        h.absorb(&SessionEvent::ToolResultLogged {
            turn: 1,
            round: 31,
            call_id: "c1".into(),
            content: (0..6)
                .map(|n| format!("result row {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
            images: Vec::new(),
        });
        h
    }

    #[test]
    fn the_frame_holds_still_while_the_conversation_scrolls() {
        // The line ADR 0020 draws, from the other side: the tail is what moves,
        // and the frame is what must not. `tip` and `input` are the reason that
        // line exists at all — the field is where a hand is already reaching,
        // and a row that pushed it down when a tip arrived would move it out
        // from under that hand (`modules/tip.rs`).
        //
        // Swept across the whole range, including past the end, because the
        // failure this guards is an off-by-one in the pane split rather than a
        // gross error.
        let h = host_with_the_shipped_tail();
        for i in 0..40 {
            h.absorb(&SessionEvent::AssistantMessage {
                turn: 1,
                round: i,
                text: format!("row {i}"),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            });
        }
        let size = (80, 24);
        let _ = h.compose(size);

        let resting = h.compose(size);
        let frame: Vec<(String, Rect)> =
            ["tip", crate::modules::input::ID, crate::modules::status::ID]
                .iter()
                .map(|id| {
                    (
                        id.to_string(),
                        resting
                            .part(id)
                            .unwrap_or_else(|| panic!("`{id}` is not on screen"))
                            .rect,
                    )
                })
                .collect();

        let limit = h.scroll_limit(size, &h.moment.read().unwrap().clone());
        for scroll in 0..=limit + 5 {
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(scroll);
            let frame_now = h.compose(size);
            for (id, want) in &frame {
                let got = frame_now
                    .part(id)
                    .unwrap_or_else(|| panic!("`{id}` left the screen at scroll {scroll}"))
                    .rect;
                assert_eq!(
                    got, *want,
                    "`{id}` moved at scroll {scroll}: {:?} was {:?}",
                    got, want
                );
            }
        }
    }

    #[test]
    fn the_sum_of_what_there_is_to_read_is_what_gets_drawn() {
        // The property the scroll bound rests on: `stream_height` has to agree
        // with the frame, or the bottom rows of the conversation cannot be
        // scrolled to. Now that the tail is part of the sum, "the frame" means
        // the blocks *and* the tail rows — and the cap has to be the same in
        // both, or the two disagree by whatever was capped away.
        //
        // Swept over scroll positions because the two are computed by different
        // walks: the sum counts rows, the frame decides which ones fit, and an
        // off-by-one shows up at the end of the range rather than the start.
        let h = host_with_the_shipped_tail();
        for i in 0..40 {
            h.absorb(&SessionEvent::AssistantMessage {
                turn: 1,
                round: i,
                text: format!("row {i}"),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            });
        }
        // A plan **taller than the pane**, so the cap is actually in force.
        // A two-item plan would leave it idle and this judgement would pass
        // against a `stream_height` that had no cap at all — which is what the
        // first version of it did.
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 40,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: "t1".into(),
                name: "todowrite".into(),
                arguments: serde_json::json!({
                    "todos": (0..40).map(|i| serde_json::json!({
                        "content": format!("item {i}"),
                        "status": if i == 0 { "in_progress" } else { "pending" },
                    })).collect::<Vec<_>>()
                })
                .to_string(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        });

        let size = (80, 24);
        let height = h.stream_height(size, &h.moment.read().unwrap().clone());
        let rows = h.stream_rows(size, &h.moment.read().unwrap().clone());
        let limit = h.scroll_limit(size, &h.moment.read().unwrap().clone());
        assert!(limit > 0, "the fixture has to be scrollable");

        // The cap has to be biting, or this judgement is about a code path the
        // fixture never reaches — which is how its first version passed against
        // a `stream_height` that had no cap at all.
        let uncapped: usize = h
            .tail_heights(size.0, &h.moment.read().unwrap().clone())
            .iter()
            .map(|(_, h)| *h as usize)
            .sum();
        assert!(
            uncapped > rows as usize,
            "the tail asks for {uncapped} rows of a {rows}-row pane: the cap is idle \
             here, so this judgement cannot see it"
        );

        // At the very bottom the frame draws the last `rows` of what there is:
        // the count and the drawing have to agree about how much that is. If
        // the sum counted rows the frame declined to draw, this is where the
        // difference lands — the tail's own rows would be out of reach.
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(0);
        let scrolled_to_top = {
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
            h.compose(size)
        };
        let drawn = scrolled_to_top
            .parts
            .iter()
            .filter(|p| p.owner == "stream" || TAIL.contains(&p.owner.as_str()))
            .map(|p| p.lines.len())
            .sum::<usize>();
        assert_eq!(
            drawn,
            rows as usize,
            "the frame drew {drawn} rows of a {rows}-row pane, but the sum says there              are {height} rows to read: what is at the top of the scroll would not be              reachable"
        );
    }

    #[test]
    fn the_tail_is_what_scrolls_and_the_frame_is_not() {
        // The line ADR 0020 draws. A module whose rows ride the tail scrolls; a
        // module in the frame has to stay where it is, because the frame holds
        // the field and the field is what a hand is reaching for.
        for (name, tree) in shipped_trees() {
            let tail = tree.tail();
            for frame_module in [
                crate::modules::input::ID,
                crate::modules::tip::ID,
                crate::modules::status::ID,
            ] {
                assert!(
                    !tail.contains(&frame_module.to_string()),
                    "`{frame_module}` must not scroll, and the {name} layout puts it in the tail"
                );
            }
            for riding in tail {
                assert!(
                    TAIL.contains(&riding.as_str()),
                    "`{riding}` rides the tail of the {name} layout but is not declared in TAIL"
                );
            }
        }
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
        fn lines(&self, _ctx: &crate::block::RenderCtx) -> Vec<crate::frame::Line> {
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

    /// 计时/等价性用的块，kind 可控。
    #[derive(Debug)]
    struct Kinded {
        kind: &'static str,
        lines: Vec<String>,
    }

    impl crate::block::Content for Kinded {
        fn kind(&self) -> &'static str {
            self.kind
        }
        fn content_hash(&self) -> crate::block::ContentHash {
            crate::block::hash_of(&self.lines.iter().map(|l| l.as_str()).collect::<Vec<_>>())
        }
        fn lines(&self, _ctx: &crate::block::RenderCtx) -> Vec<crate::frame::Line> {
            self.lines
                .iter()
                .map(|l| crate::frame::Line::raw(l.as_str()))
                .collect()
        }
    }

    /// **索引必须说出走动说出的话。**
    ///
    /// The row index is only safe to trust because of this test. It replaces a
    /// direct walk that computed, per slot, three things — is it hidden, does a
    /// lid cover it, how many rows does it measure — with a table kept across
    /// frames and re-used whenever it *looks* like nothing changed. "Looks like"
    /// is where a cache earns its reputation: the failure mode is not a wrong
    /// answer for the slot that changed, it is a stale answer for the slots
    /// around it, and it shows up as a scroll that cannot reach the bottom or a
    /// row that stopped being drawn.
    ///
    /// Two comparisons, because they fail differently:
    ///
    /// * **against the walk** — the arithmetic, which catches a `lid_row` that
    ///   drifted from the loop it replaced.
    /// * **against a forced rebuild** — the reuse, which catches an index that
    ///   kept a measurement the stream had since invalidated. This is the one a
    ///   walk comparison alone cannot see, because both sides would be the same
    ///   stale table.
    ///
    /// Driven through every change that can move a row count, because the
    /// revisions are only worth having if the mutations actually reach them:
    /// a streamed chunk, a settle, a kind folded, a kind hidden, one block
    /// folded by hand, and a resize.
    #[test]
    fn the_row_index_says_what_the_walk_says() {
        let h = host();
        let width = 80u16;
        let ctx = crate::block::RenderCtx::bare(width);
        let check = |label: &str| {
            let stream = h.stream.read().unwrap();
            let pres = h.presentation.read().unwrap();
            let slots = stream.slots();
            let walk = h.rows_by_walk(&ctx, slots, &pres);
            let kept = h
                .row_index(&ctx, slots, &pres, crate::moment::Activity::Working)
                .rows
                .clone();
            h.forget_row_index();
            let fresh = h
                .row_index(&ctx, slots, &pres, crate::moment::Activity::Working)
                .rows
                .clone();
            assert_eq!(
                kept, walk,
                "{label}: the kept index disagrees with a fresh walk — a reuse \
                 survived something that changed it"
            );
            assert_eq!(
                fresh, walk,
                "{label}: a from-scratch index disagrees with the walk — the \
                 arithmetic drifted from the loop it replaced"
            );
        };

        // A block of each kind the fold table has an opinion about, plus two
        // prose blocks so there is a seam to keep.
        {
            let mut s = h.stream.write().unwrap();
            let mut w = s.writer("bench");
            for (i, kind) in ["assistant", "reasoning", "tool_call", "assistant"]
                .into_iter()
                .enumerate()
            {
                w.emit(
                    crate::block::Coord::default(),
                    Arc::new(Kinded {
                        kind,
                        lines: vec![format!("{kind} {i}"), "second row".into()],
                    }),
                );
            }
        }
        check("initial");

        // A block still arriving: the count for this slot is not cacheable.
        let live = {
            let mut s = h.stream.write().unwrap();
            let mut w = s.writer("bench");
            w.open(
                crate::block::Coord::default(),
                Arc::new(Kinded {
                    kind: "assistant",
                    lines: vec!["growing".into()],
                }),
            )
        };
        check("open");

        // …amended, which is what a streamed chunk does — and which must NOT
        // invalidate the whole table, since it happens per token.
        {
            let mut s = h.stream.write().unwrap();
            let mut w = s.writer("bench");
            let widened: Vec<String> = (0..5).map(|i| format!("grown row {i}")).collect();
            assert!(w.amend(
                live,
                Arc::new(Kinded {
                    kind: "assistant",
                    lines: widened,
                })
            ));
        }
        check("amended");

        {
            let mut s = h.stream.write().unwrap();
            let mut w = s.writer("bench");
            assert!(w.settle(live));
        }
        check("settled");

        // The three presentation routes, each its own revision.
        h.presentation.write().unwrap().toggle("assistant");
        check("kind folded");
        h.presentation.write().unwrap().toggle("reasoning");
        check("kind unfolded");
        h.presentation.write().unwrap().set_block(live, true);
        check("block folded");

        // A different width answers a different question for every slot.
        let stream = h.stream.read().unwrap();
        let pres = h.presentation.read().unwrap();
        let narrow = 40u16;
        let narrow_ctx = crate::block::RenderCtx::bare(narrow);
        let walk = h.rows_by_walk(&narrow_ctx, stream.slots(), &pres);
        let kept = h
            .row_index(
                &narrow_ctx,
                stream.slots(),
                &pres,
                crate::moment::Activity::Working,
            )
            .rows
            .clone();
        assert_eq!(kept, walk, "resize: the index was not re-measured");
    }

    #[test]
    fn zz_measure_after_index() {
        use std::time::Instant;
        let Ok(path) = std::env::var("ATOMCODE_PERF_LOG") else {
            return;
        };
        let h = host();
        use std::io::BufRead;
        for line in std::io::BufReader::new(std::fs::File::open(&path).unwrap()).lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() || line.contains("\"header\"") {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(ev) = rec.get("event") else { continue };
            let Ok(fact) = serde_json::from_value::<SessionEvent>(ev.clone()) else {
                continue;
            };
            h.absorb(&fact);
        }
        let size = (80u16, 24u16);
        let m = h.moment.read().unwrap().clone();
        let t = Instant::now();
        let limit = h.scroll_limit(size, &m);
        let cold = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        for _ in 0..10 {
            h.scroll_limit(size, &m);
        }
        let warm = t.elapsed().as_secs_f64() * 1000.0 / 10.0;
        println!(
            "\n槽位 {} / scroll_limit 冷 {cold:.3}ms 热 {warm:.3}ms",
            h.stream.read().unwrap().slots().len()
        );
        for (label, scroll) in [("底部", 0usize), ("中部", limit / 2), ("顶部", limit)] {
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(scroll);
            let _ = h.compose(size);
            let t = Instant::now();
            for _ in 0..10 {
                let _ = h.compose(size);
            }
            println!(
                "  compose @{label:<4} {:.3}ms/帧",
                t.elapsed().as_secs_f64() * 1000.0 / 10.0
            );
        }
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
        let t = Instant::now();
        for i in 0..10 {
            let mm = h.moment.read().unwrap().clone();
            let max = h.scroll_limit(size, &mm);
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(max.saturating_sub(i));
            let _ = h.compose(size);
        }
        println!(
            "  一格滚轮 {:.3}ms",
            t.elapsed().as_secs_f64() * 1000.0 / 10.0
        );
    }

    /// 真实日志下的帧成本。`ATOMCODE_PERF_LOG` 不给就跳过，所以它不是门，
    /// 是一把尺子 —— 只在有人要量的时候量。
    #[test]
    fn zz_measure_real_session() {
        use std::time::Instant;
        let Ok(path) = std::env::var("ATOMCODE_PERF_LOG") else {
            return;
        };
        let h = host();
        use std::io::BufRead;
        for line in std::io::BufReader::new(std::fs::File::open(&path).unwrap()).lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() || line.contains("\"header\"") {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(ev) = rec.get("event") else { continue };
            let Ok(fact) = serde_json::from_value::<SessionEvent>(ev.clone()) else {
                continue;
            };
            h.absorb(&fact);
        }
        let size = (80u16, 24u16);
        let m = h.moment.read().unwrap().clone();
        let t = Instant::now();
        let limit = h.scroll_limit(size, &m);
        let cold = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        for _ in 0..20 {
            h.scroll_limit(size, &m);
        }
        let warm = t.elapsed().as_secs_f64() * 1000.0 / 20.0;
        println!(
            "\n槽位 {} / scroll_limit: 冷 {cold:.3}ms, 热 {warm:.3}ms",
            h.stream.read().unwrap().slots().len()
        );
        for (label, scroll) in [("底部", 0usize), ("中部", limit / 2), ("顶部", limit)] {
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(scroll);
            let _ = h.compose(size);
            let t = Instant::now();
            for _ in 0..20 {
                let _ = h.compose(size);
            }
            println!(
                "  compose @{label:<4} {:.3}ms/帧",
                t.elapsed().as_secs_f64() * 1000.0 / 20.0
            );
        }
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
        let t = Instant::now();
        for i in 0..20 {
            let mm = h.moment.read().unwrap().clone();
            let max = h.scroll_limit(size, &mm);
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(max.saturating_sub(i));
            let _ = h.compose(size);
        }
        println!(
            "  一格滚轮 {:.3}ms",
            t.elapsed().as_secs_f64() * 1000.0 / 20.0
        );
    }

    /// **一把尺子，不是一道闸门。** 给它 `ATOMCODE_PERF_LOG=<真实会话的 jsonl>`
    /// 就把那份会话折进一个 host，量一格滚轮的真实成本；不给就立刻返回。
    ///
    /// 它存在的理由：这份 host 里其他每一条判断都是「画了什么」——而「一帧花了
    /// 多久」在画面上看不出来，同样的行两种做法都画得出来。`COPIED_ROWS` 与
    /// `block::LIVE_RESUMES` 是同一种东西，只是那两个数**调用次数**，这个数
    /// **时间**。
    ///
    /// ## 2026-09-15 的实测（用户报「滚动卡、CPU 99%」）
    ///
    /// 会话 1570 个槽位。**一格滚轮 = `scroll_limit` + `compose`**：
    ///
    /// ```text
    ///              debug    release
    /// 一格滚轮      20.7ms    5.5ms      ← 差 4 倍
    /// compose@顶部  20.4ms    5.3ms      ← 最坏：视口在最旧的内容处
    /// compose@底部   1.0ms    0.2ms
    /// scroll_limit   4.1ms    0.11ms
    /// ```
    ///
    /// **结论：那是构建配置，不是算法。** 用户跑的是 `./target/debug/atui`；
    /// 换 release 即缓解。所以量这里的东西必须说清用哪个构建，否则会把 debug
    /// 的放大倍数当成算法问题。
    ///
    /// 成本随**槽位数**走，不随日志大小走 —— 381774 个事件只折出 1570 个块
    /// （`assistant_chunk` 累积成一块），所以 427MB 的会话和 30MB 的会话在帧
    /// 成本上几乎一样。看日志大小估帧成本会估错一个数量级。
    ///
    /// ## 两处优化，都在树里
    ///
    /// **一、行数索引。** 「两条走动各自把整段会话的每块行数重算一遍」是对的猜
    /// 想。按 `(stream 版本, 呈现版本, 宽度)` 键控的前缀/后缀和，两条走动共读：
    ///
    /// ```text
    ///                接线前     接线后
    /// scroll_limit    2.04ms →  0.12ms    ← 17 倍
    /// compose@顶部     5.33ms →  5.37ms    ← 无差异，它慢在别处
    /// ```
    ///
    /// 收益最大的一处不在滚轮上：`pinned` 在读者往回滚着看历史时**每个 chunk 调
    /// 两次** `stream_height_in`，所以「边跑边回看大段历史」是 4.1ms → 0.24ms
    /// 每 chunk。
    ///
    /// 换来 2 处真回归（隐藏槽位被压成 0、已折叠块拒绝响应点击），都被既有测试
    /// 与棘轮 `the_row_index_says_what_the_walk_says` 抓到并修好。
    ///
    /// **二、`stream_lines` 的二分跳转。** 真正的成本在这里：它从最新一块**倒着
    /// 走到视口**，是 O(槽位)，1570 个槽位时为了一屏 24 行要迭代 1546 次。索引
    /// 里的后缀和（`RowIndex::skip_from`）让边界成了一个 partition point。
    ///
    /// ```text
    ///             索引后    跳转后     降幅
    /// 一格滚轮     5.54ms →  0.273ms   20×   （release）
    /// 一格滚轮    20.7ms  →  0.720ms   29×   （debug）
    /// compose@顶部 5.37ms →  0.144ms   37×
    /// ```
    ///
    /// 跳转必须自己算对两样东西：走动能累积多少（`skipped`）和它手里握着哪一块
    /// 的 kind（`below`）。**第一版在第二样上错了** —— 用前缀和反推，差一个空
    /// 行，视口低一行、最开始那句话被顶出屏幕。所以索引存的是**走动自己的那个
    /// 数**（后缀和），而不是让人再换算一次。棘轮
    /// `the_jump_draws_what_the_walk_draws` 把每个滚动位置都比一遍。
    ///
    /// ## 量这个必须核对对照物本身
    ///
    /// 第一次量「索引有没有用」时，我把「接线前」的备份和「接线后」相比，得出
    /// 「release 下收益为零」并据此撤掉了这份改动 —— 而**那个备份里已经带着接
    /// 线**，等于同一版本跟自己比。教训：性能对照要么干净重建，要么先确认两份
    /// 代码真的不同。
    ///
    /// 另外，**构建必须说清**：同一条路径 debug 比 release 慢约 4 倍，所以
    /// 「20ms 卡顿」在 release 下可能只是 5ms。用户报 CPU 99% 时跑的是
    /// `./target/debug/atui`。
    #[test]
    fn measure_the_frame_cost_of_a_real_session() {
        use std::time::Instant;
        let Ok(path) = std::env::var("ATOMCODE_PERF_LOG") else {
            // The normal case. A ruler nobody picked up says nothing and must
            // not fail — see the doc above for why this is not a gate.
            return;
        };
        let h = host();
        use std::io::BufRead;
        for line in
            std::io::BufReader::new(std::fs::File::open(&path).expect("open the log")).lines()
        {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() || line.contains("\"header\"") {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(ev) = rec.get("event") else { continue };
            let Ok(fact) = serde_json::from_value::<SessionEvent>(ev.clone()) else {
                continue;
            };
            h.absorb(&fact);
        }
        // A fixture that loaded nothing would print a flattering number.
        let slots = h.stream.read().unwrap().slots().len();
        assert!(
            slots > 0,
            "{path} folded into no blocks at all, so every figure below would be \
             the cost of an empty screen"
        );

        let size = (80u16, 24u16);
        let m = h.moment.read().unwrap().clone();
        let t = Instant::now();
        let limit = h.scroll_limit(size, &m);
        let cold = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        for _ in 0..20 {
            h.scroll_limit(size, &m);
        }
        let warm = t.elapsed().as_secs_f64() * 1000.0 / 20.0;
        println!("\n槽位 {slots} / scroll_limit: 冷 {cold:.3}ms, 热 {warm:.3}ms");

        for (label, scroll) in [("底部", 0usize), ("中部", limit / 2), ("顶部", limit)] {
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(scroll);
            let _ = h.compose(size);
            let t = Instant::now();
            for _ in 0..20 {
                let _ = h.compose(size);
            }
            println!(
                "  compose @{label:<4} {:.3}ms/帧",
                t.elapsed().as_secs_f64() * 1000.0 / 20.0
            );
        }

        // One wheel notch, as the front end actually pays for it: bound the
        // scroll, then paint.
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
        let t = Instant::now();
        for i in 0..20 {
            let mm = h.moment.read().unwrap().clone();
            let max = h.scroll_limit(size, &mm);
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(max.saturating_sub(i));
            let _ = h.compose(size);
        }
        println!(
            "  一格滚轮 {:.3}ms",
            t.elapsed().as_secs_f64() * 1000.0 / 20.0
        );
    }

    /// **跳转画出来的，必须和逐格走动画出来的一模一样。**
    ///
    /// `stream_lines` 从最新一块倒着走到视口。二分跳转让它直接落到视口所在
    /// 槽位 —— release 下把一帧从 5.37ms 压到 0.145ms，debug 下从 20.4ms 压到
    /// 0.39ms，1570 个槽位。省掉的正是那 1546 次「确定要跳过的槽位」的迭代。
    ///
    /// 代价是跳转必须自己算对两样东西：走动能累积多少（`skipped`），以及它手
    /// 里握着的是哪一块的 kind（`below`，块与块之间的空行归谁看它）。**第一版
    /// 就在第二样上错了** —— 用前缀和反推，差了一个空行，视口低一行、最开始那
    /// 句话被顶出屏幕。既有测试抓到了，但只有一条，覆盖的是一个位置。
    ///
    /// 所以这条棘轮把**每一个滚动位置**都比一遍：两个腿跑同一个 host，逐行比
    /// 对，连归属（`owner`，决定点击折叠哪一块）一起比。跳转只在某些位置生效
    /// 正是危险所在 —— 差异会藏在「恰好没被覆盖的那几个 scroll 值」上。
    #[test]
    fn the_jump_draws_what_the_walk_draws() {
        use std::sync::atomic::Ordering as O;
        // A stream with everything the jump has to reason about: prose (seams
        // between blocks), reasoning (hidden by default → draws nothing and is
        // not a neighbour), tool calls (merged behind lids), and a kind folded to
        // one row.
        let h = host();
        {
            let mut s = h.stream.write().unwrap();
            let mut w = s.writer("bench");
            // Long enough to scroll well past a screenful, and mixed enough that
            // the jump has to reason about all three of its cases: prose (seams),
            // a kind that draws nothing, and tool calls merged behind lids.
            for i in 0..60usize {
                let kind = ["assistant", "reasoning", "tool_call", "assistant"][i % 4];
                let title = if kind == "tool_call" {
                    "read_file"
                } else {
                    "note"
                };
                w.emit(
                    crate::block::Coord::default(),
                    Arc::new(Kinded {
                        kind,
                        lines: (0..(2 + i % 4))
                            .map(|r| format!("{title} {i} row {r} 内容"))
                            .collect(),
                    }),
                );
            }
        }
        let size = (80u16, 24u16);
        let limit = h.scroll_limit(size, &h.moment.read().unwrap().clone());
        assert!(
            limit > 4,
            "the fixture must be scrollable, got limit {limit}"
        );

        // The whole frame, and the per-row attribution a click reads. The second
        // is not redundant: `below` is what the jump has to reconstruct, and it
        // decides both the seams on screen and which block a click on a row
        // folds — a jump that got the rows right and the neighbours wrong would
        // pass on the first and fail on the second.
        let snapshot = |scroll: usize| {
            h.moment.write().unwrap().scroll = crate::moment::ScrollPos(scroll);
            let frame = h.compose(size);
            let rows = h.hits.lock().unwrap().rows.clone();
            (frame, rows)
        };

        // Both legs, every position — and a few past the limit, because the front
        // end clamps but the walk should not care either way.
        for scroll in 0..=limit + 3 {
            crate::host::NO_JUMP.store(true, O::SeqCst);
            let walked = snapshot(scroll);
            crate::host::NO_JUMP.store(false, O::SeqCst);
            let jumped = snapshot(scroll);
            assert_eq!(
                walked.0, jumped.0,
                "at scroll {scroll} the jump drew a different screen than the \
                 walk — same host, so this is the jump's arithmetic and not the \
                 content's"
            );
            assert_eq!(
                walked.1, jumped.1,
                "at scroll {scroll} the jump attributed the rows differently, so \
                 a click would fold a different block"
            );
        }
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
        let first = h.stream_height((80, 24), &h.moment.read().unwrap().clone());
        assert!(first > 0);
        let measured = asked.load(orders);
        assert!(
            measured <= 20,
            "the first walk measured {measured} blocks for a 20-block session"
        );

        asked.store(0, orders);
        for _ in 0..5 {
            let _ = h.stream_height((80, 24), &h.moment.read().unwrap().clone());
        }
        assert_eq!(
            asked.load(orders),
            0,
            "asking again how tall settled blocks are rendered them again"
        );
    }

    // ---- pinning, when the thing that grew is a module rather than a block

    #[test]
    fn a_shift_in_what_the_tail_draws_is_pinned_too() {
        // The tail's height has more than one source, and they arrive by
        // different routes: a fact goes through `absorb`, while
        // `Moment::activity` is written by the event loop and never through
        // `absorb` at all. Which of the two lands first is not settled anywhere,
        // so a pin inside `absorb` alone holds for half the moves — the screen
        // jumps on some turns and not others, which is worse than not holding.
        //
        // This drives the route that does not go through `absorb`.
        let h = host_with_peek_tail();
        let size = (80, 24);
        fed_and_scrollable(&h);

        // First, at the bottom, the instrument itself: it is the thing whose
        // height moves, and if it cannot move this test judges nothing.
        let _ = h.compose(size);
        let rest = h.stream_height((80, 24), &h.moment.read().unwrap().clone());
        h.set_activity(crate::moment::Activity::Working);
        let working = h.stream_height((80, 24), &h.moment.read().unwrap().clone());
        assert_eq!(
            working,
            rest + PEER_ROWS as usize,
            "the instrument does not change the sum, so nothing here is being judged"
        );
        assert!(
            h.compose(size).part("peek").is_some(),
            "and it does not reach the screen"
        );

        // Now the claim: a reader holding a position is not moved by it. The
        // scroll is measured from the bottom of the pane, so the two new rows
        // have to be added to it or the same conversation slides down under
        // the same eyes.
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(5);
        let before = h.compose(size);
        let rows_before: Vec<String> = lines_of(&before, "stream");

        h.set_activity(crate::moment::Activity::Idle);
        let after = h.compose(size);

        assert_eq!(
            h.moment.read().unwrap().scroll.0,
            5 - PEER_ROWS as usize,
            "the tail went away and the reading was not moved to match"
        );
        assert_eq!(
            lines_of(&after, "stream"),
            rows_before,
            "the same rows have to stay under the same eyes"
        );

        // And back the other way, which is the half a one-directional pin
        // misses: the offset has to come *down* when the tail leaves.
        let back = h.compose(size);
        let rows_at_rest: Vec<String> = lines_of(&back, "stream");
        h.set_activity(crate::moment::Activity::Working);
        let grown = h.compose(size);
        assert_eq!(
            h.moment.read().unwrap().scroll.0,
            5 - PEER_ROWS as usize + PEER_ROWS as usize,
            "the tail came back without the reading being moved"
        );
        assert_eq!(
            lines_of(&grown, "stream"),
            rows_at_rest,
            "and the rows under the eyes are the ones that were there"
        );
    }

    fn lines_of(frame: &Frame, part: &str) -> Vec<String> {
        frame
            .part(part)
            .unwrap_or_else(|| panic!("no `{part}` in the frame"))
            .lines
            .iter()
            .map(|l| l.plain())
            .collect()
    }

    #[test]
    fn a_pin_that_would_leave_the_top_empty_is_pulled_back() {
        // The other side of the same coin: pinned past the new limit, the
        // scroll would sit above the oldest line — which reads as the
        // conversation having been lost, the failure `scroll_limit` exists to
        // prevent.
        let h = host_with_peek_tail();
        let size = (80, 24);
        fed_and_scrollable(&h);
        let _ = h.compose(size);

        // Right at the top, where there is no room left to give.
        let limit = h.scroll_limit(size, &h.moment.read().unwrap().clone());
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
        let _ = h.compose(size);

        // A tail arrives, which adds rows there is to read — this is the
        // direction that would push the offset past the top if nothing clamped
        // it.
        h.set_activity(crate::moment::Activity::Working);

        let now = h.moment.read().unwrap().clone();
        assert!(
            now.scroll.0 <= h.scroll_limit(size, &now),
            "scrolled to {} with only {} to read",
            now.scroll.0,
            h.scroll_limit(size, &now)
        );
    }

    // ---- the pane's split between the conversation and its tail ----------
    //
    // `pane_geometry` has no caller with a non-empty tail until ADR 0020's
    // Step 3, so these are the only thing standing between the arithmetic and
    // the day it is switched on. They are written against the numbers in the
    // plan, which were worked out on a 21-row pane: 100 rows of blocks, a 4-row
    // todo, a 2-row live line.

    fn pane_heights() -> Vec<(String, u16)> {
        vec![("todo".to_string(), 4), ("live".to_string(), 2)]
    }

    #[test]
    fn a_capped_tail_keeps_the_live_line_and_cuts_the_plan() {
        // **Who** gives up the rows, not just how many. `cap_tail` rations from
        // the top, so the plan loses rows and the line against the bottom keeps
        // its two — which is the right way round: the task list has a `window`
        // for running out of room (`+N 更多`), and the live line has nothing to
        // fall back on. It is the row that answers "is it stuck?".
        //
        // The loop used to go the other way while the comment said this, so a
        // long plan in a short terminal emptied the live line first.
        let pane_h = 19u16;
        let capped = Host::cap_tail(
            vec![("todo".to_string(), 40), ("live".to_string(), 2)],
            pane_h,
        );
        assert_eq!(
            capped[1].1, 2,
            "the live line was cut: {capped:?} — a turn in flight would have no row"
        );
        assert!(
            capped[0].1 + capped[1].1 < pane_h,
            "the cap left no room for the conversation: {capped:?}"
        );

        // Rationing is top-down and it stops as soon as it is under budget: the
        // module nearest the top gives up everything it has before the one below
        // it is touched. `todo` is the topmost, so with a plan that is enormous
        // and a third module declared below the live line, the plan absorbs the
        // whole cut and the other two keep every row.
        let three = Host::cap_tail(
            vec![
                ("todo".to_string(), 40),
                ("live".to_string(), 2),
                ("extra".to_string(), 8),
            ],
            pane_h,
        );
        assert_eq!(three[1].1, 2, "the live line was cut: {three:?}");
        assert_eq!(
            three[2].1, 8,
            "a module *below* the live line was cut before the live line was: {three:?}"
        );
        assert_eq!(three[0].1, 8, "the plan took the whole cut: {three:?}");
    }

    #[test]
    fn a_tail_taller_than_the_pane_still_scrolls() {
        // Handing the module its visible height means a module taller than the
        // pane is asked for the whole pane at several different offsets, and
        // lays itself out identically each time: the reader turns the wheel and
        // the picture does not move, while the badge counts up. That is the
        // shape of a frozen screen, and the cap is what stops it.
        let pane = Rect::sized(80, 19);
        // A plan far taller than the pane it has to fit in.
        let heights = vec![("todo".to_string(), 40u16), ("live".to_string(), 2)];

        let capped = Host::cap_tail(heights.clone(), pane.h);
        let total: u16 = capped.iter().map(|(_, h)| *h).sum();
        assert!(
            total < pane.h,
            "the tail took {total} rows of a {}-row pane, so nothing scrolls",
            pane.h
        );

        // And the scroll really does move the picture, one row per row.
        let mut previous: Option<u16> = None;
        for scroll in 0..total as usize {
            let p = Host::pane_geometry(pane, scroll, &capped);
            let drawn: u16 = p.tail.iter().map(|(_, r)| r.h).sum();
            assert!(
                previous.is_none_or(|before| drawn < before),
                "scroll {scroll} drew {drawn} tail rows, the same as before — frozen"
            );
            previous = Some(drawn);
        }
    }

    #[test]
    fn with_no_tail_the_split_is_the_identity() {
        // Every layout this build ships. If this stopped holding, nothing would
        // compose the way it did before the split existed — which is the whole
        // reason Step 2 is allowed to be behaviour-preserving.
        let pane = Rect::sized(80, 21);
        for scroll in [0usize, 1, 7, 1000] {
            let p = Host::pane_geometry(pane, scroll, &[]);
            assert_eq!(
                p.block_rect, pane,
                "scroll {scroll}: the pane is the blocks'"
            );
            assert_eq!(p.block_scroll, scroll, "scroll {scroll}: nothing took rows");
            assert!(p.tail.is_empty());
        }
    }

    #[test]
    fn the_tail_is_what_scrolls_away_first() {
        // The point of the whole exercise: scrolling back eats the tail off the
        // bottom before it touches the conversation's *offset*. Six tail rows,
        // so scroll 0 to 6 leaves `block_scroll` at zero — the blocks are not
        // moving up through the content, though they do move down the screen as
        // the rect they sit in grows (see the ADR: those are two different
        // statements and only one of them is true).
        let pane = Rect::sized(80, 21);
        for scroll in 0..=6usize {
            let p = Host::pane_geometry(pane, scroll, &pane_heights());
            assert_eq!(p.block_scroll, 0, "scroll {scroll} moved the conversation");
            assert_eq!(
                p.block_rect.h,
                15 + scroll as u16,
                "scroll {scroll}: the rows the tail gave back did not go to the conversation"
            );
            assert_eq!(
                p.block_rect.h as usize + 6 - scroll,
                21,
                "scroll {scroll}: the pane stopped adding up"
            );
        }
        // And only then do the blocks start to move.
        let p = Host::pane_geometry(pane, 7, &pane_heights());
        assert_eq!(p.block_scroll, 1, "past the tail, the conversation scrolls");
        assert_eq!(
            p.block_rect.h, 21,
            "with the tail gone it has the whole pane"
        );
        assert!(p.tail.is_empty());
    }

    #[test]
    fn a_tail_module_scrolls_out_one_row_at_a_time() {
        // Not "disappears": four rows of todo lose a row at a time, and what is
        // left keeps its place against the bottom. This is what the live line's
        // whole-or-nothing `showing()` could not do.
        let pane = Rect::sized(80, 21);
        let h = |scroll: usize, id: &str| -> Option<Rect> {
            Host::pane_geometry(pane, scroll, &pane_heights())
                .tail
                .into_iter()
                .find(|(i, _)| i == id)
                .map(|(_, r)| r)
        };

        // At rest both are up, the last id lowest: live sits under todo.
        let todo = h(0, "todo").expect("todo is up");
        let live = h(0, "live").expect("live is up");
        assert_eq!(todo, Rect::new(0, 15, 80, 4), "todo, above the live line");
        assert_eq!(live, Rect::new(0, 19, 80, 2), "live, against the bottom");
        assert_eq!(live.bottom(), 21, "the newest thing is at the foot");

        // Scroll 1 takes the bottom row off `live` — and takes it off the
        // bottom *edge*, so what is left of live is the row still at y=20, not
        // a shrunken box floating above it. Which row of live that is does not
        // matter to the geometry: the module is handed a one-row rect and lays
        // itself out in it, which is why live keeps saying what it is doing
        // instead of going blank the moment it is clipped.
        assert_eq!(h(1, "live"), Some(Rect::new(0, 20, 80, 1)));
        assert_eq!(h(2, "live"), None, "two rows of live, two rows of scroll");

        // Then todo starts to go, from its bottom row up.
        assert_eq!(h(3, "todo"), Some(Rect::new(0, 18, 80, 3)), "one row gone");
        assert_eq!(h(6, "todo"), None, "and the rest with it");
    }

    #[test]
    fn a_tail_module_is_asked_for_the_rows_it_has_left() {
        // The rect handed to the module is its *visible* height, not its full
        // height clipped: `todo::window` and `live::render` both lay themselves
        // out differently when they have less room, and cutting a full-height
        // render is what would take away the row each of them keeps first.
        let pane = Rect::sized(80, 21);
        let p = Host::pane_geometry(pane, 3, &pane_heights());
        let todo = p.tail.iter().find(|(i, _)| i == "todo").expect("todo");
        assert_eq!(todo.1.h, 3, "asked for three rows, not given four and cut");
    }

    #[test]
    fn the_tail_counts_towards_what_there_is_to_read() {
        // Not counted, its own rows would be out of reach at the bottom of the
        // scroll — the same failure the block walk goes to lengths to avoid for
        // a reply that wraps wider than it is measured.
        //
        // The layout is swapped for one that really declares a tail, because
        // every layout this build ships declares none: the sum's tail term has
        // no other way to be exercised before Step 3 turns one on.
        let h = host();
        let moment = Moment::default();
        // Mounted, or `tail_heights` skips it: an id nobody answers for is not a
        // row of anything.
        h.modules
            .add_view(Arc::new(
                crate::module::Mounted::<crate::modules::todo::Todo>::new(),
            ))
            .unwrap();
        // A plan of two items, folded the way the real thing arrives: an
        // assistant message carrying the call. `todo` is a view module with no
        // producer, so this is what puts rows in it — not a block emit.
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: "t1".into(),
                name: "todowrite".into(),
                arguments: serde_json::json!({
                    "todos": [
                        { "content": "first", "status": "in_progress" },
                        { "content": "second", "status": "pending" },
                    ]
                })
                .to_string(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        });

        // The same state, measured twice: only the declaration differs, so the
        // difference is the tail's own rows and nothing else. Comparing across
        // the `absorb` above would have compared two different conversations —
        // the message it folds is itself a block.
        let declared = |with_tail: bool| {
            let pane = |tail: Vec<&str>| {
                Region::split(
                    crate::region::Dir::Vertical,
                    crate::region::Constraint::Fill,
                    Region::stream().with_tail(tail),
                    Region::view(crate::modules::status::ID),
                )
            };
            h.layout.set(if with_tail {
                pane(vec!["todo"])
            } else {
                pane(Vec::new())
            });
            h.stream_height_in(h.stream_room((80, 24), &moment), &moment)
        };

        let without = declared(false);
        assert_eq!(without.1, 0, "nothing declared, so nothing is added");

        let with = declared(true);
        assert_eq!(
            with.1, 4,
            "a plan of two items is a margin, a header and two rows, and it counts"
        );
        assert_eq!(
            with.0,
            without.0 + 4,
            "the tail's rows are in the sum, not only beside it"
        );
        assert_eq!(
            h.stream_height((80, 24), &moment),
            with.0,
            "`stream_height` is the total, tail included"
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
        assert!(text.contains("ReadFile"), "the tools it used");
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
        // own each either. The run is drawn as one lid, one row once it is over.
        let h = fed();
        // Tool calls open by default now; ask for the merge lid explicitly, so
        // this test says which of the three modes it is about.
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Group);
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
            .position(|r| r.contains("已执行了 2 个工具"))
            .expect("the run's lid");
        assert!(
            !rows[count + 1..count + 3]
                .iter()
                .any(|r| r.contains("ReadFile")),
            "a call of the run is still drawn under the lid: {:?}",
            &rows[count..count + 3]
        );
    }

    #[test]
    fn a_run_of_folded_calls_is_one_lid_that_says_how_many() {
        // 「合并工具块」. A run of calls is one piece of work, and four rows that
        // each said nothing are four rows of noise. Once the run is over the lid
        // says how many there were — and nothing else: the commands were on
        // screen while they ran, and the folded form is how much work there
        // was, not the last command a second time.
        let h = fed();
        // Tool calls open by default now; ask for the merge lid explicitly, so
        // this test says which of the three modes it is about.
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Group);
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
            .position(|r| r.contains("已执行了 2 个工具"))
            .expect("the lid does not say how many calls there were");
        assert!(
            !rows.iter().any(|r| r.contains("ReadFile(a.rs)")),
            "the first call is still on the screen, so nothing merged:\n{rows:#?}"
        );
        // The lid is one row, and it is the count's: no command and no result
        // of the last call ride under it.
        assert!(
            !rows[count + 1..count + 3]
                .iter()
                .any(|r| r.contains("ReadFile(b.rs)") || r.contains("失败")),
            "the last call is drawn under the count: {:?}",
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
        // Tool calls open by default now; ask for the merge lid explicitly, so
        // this test says which of the three modes it is about.
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Group);
        let size = (80, 40);
        let before = h.compose(size).rows().join("\n");
        assert!(
            before.contains("已执行了 2 个工具"),
            "nothing merged:\n{before}"
        );

        let rect = h.compose(size).part("stream").unwrap().rect;
        // The count row is the lid's, and a click on it lands on the last call.
        let (id, kind) = (rect.y..rect.bottom())
            .filter_map(|y| h.block_at(2, y))
            .next()
            .expect("the lid answers a click");
        h.toggle_block(id, kind);

        let after = h.compose(size).rows().join("\n");
        assert!(
            !after.contains("已执行了 2 个工具"),
            "the lid is still drawn after the click:\n{after}"
        );
        // Every call of the run is open: its command on one row and its result on
        // the next. A call left folded would have put the result on the command's
        // own row — which is exactly the difference this click has to make, and
        // the reason a weaker assertion here would pass while one of the two
        // calls was still behind a lid.
        let open = h
            .compose(size)
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>();
        for (name, result) in [
            ("ReadFile(a.rs)", "fn main() {}"),
            ("ReadFile(b.rs)", "no such file"),
        ] {
            let head = open
                .iter()
                .position(|r| r.contains(name))
                .unwrap_or_else(|| panic!("{name} never appeared:\n{after}"));
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
        // The corpus's turns are the seam: turn 1's two calls are one lid, and
        // turn 2's lone call is drawn as itself — a lid that swallowed it would
        // report `3 个工具` over two turns' work.
        let h = fed();
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Group);
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("已执行了 2 个工具")),
            "turn 1's two calls are not one lid:\n{rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("3 个工具")),
            "a lid swallowed a call from another turn:\n{rows:#?}"
        );
        // The lone call is drawn as itself, with no count over it.
        assert!(
            rows.iter().any(|r| r.contains("看看那个目录")),
            "the second turn's call is missing:\n{rows:#?}"
        );
    }

    #[test]
    fn prose_between_calls_ends_the_run_and_keeps_its_place() {
        // A lid is drawn at the run's last call, so a run drawn across the
        // model's own words would lift them above the lid — said, to all
        // appearances, before any of the work around them. The prose ends the
        // run precisely so it stays in the order it happened in: call, words,
        // call — each side its own lid.
        let h = fed();
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Group);
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
            .position(|r| r.contains("Fixed it. 中文也要能画"))
            .expect("the model's words are on screen");
        let lids_before = rows[..prose]
            .iter()
            .filter(|r| r.contains("个工具"))
            .count();
        let lids_after = rows[prose..]
            .iter()
            .filter(|r| r.contains("个工具"))
            .count();
        assert!(
            lids_before > 0 && lids_after == 0,
            "the prose does not sit after the turn's only run: before={lids_before} \
             after={lids_after}\n{rows:#?}"
        );
    }

    #[test]
    fn the_run_stays_live_until_the_turn_ends_not_until_the_call_does() {
        // 折叠成一行是为了展示简洁的历史；还在跑的东西不是历史。所以收敛
        // 的判据是回合是否还在进行、run 后面有没有可见块，而不是最后一个
        // 调用自己完成没有——A 完成、B 还没开始时，A 的执行仍应两行展示，
        // 直到回合结束才收敛成计数。
        let h = host();
        h.set_activity(crate::moment::Activity::Working);
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Group);
        let call = |id: &str| SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: id.into(),
                name: "read_file".into(),
                arguments: format!(r#"{{"file_path":"{id}.rs"}}"#),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        };
        let result = |id: &str, text: &str| SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: id.into(),
            content: text.into(),
            is_error: false,
            images: Vec::new(),
        };
        // Two calls, both finished: the last call's result is on screen and
        // no count anywhere — the turn is still going, so the run is not
        // history yet.
        h.absorb(&call("c1"));
        h.absorb(&result("c1", "one"));
        h.absorb(&call("c2"));
        h.absorb(&result("c2", "two"));
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("ReadFile(c2.rs)")),
            "the finished call is not shown while the turn is still going:\n{rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("个工具")),
            "the run collapsed to history while the turn is still going:\n{rows:#?}"
        );

        // The turn ends: nothing is in flight any more, and the run is
        // history — the count.
        h.set_activity(crate::moment::Activity::Idle);
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("已执行了 2 个工具")),
            "the finished turn did not collapse to the count:\n{rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("ReadFile(c2.rs)")),
            "the last call is still drawn after the turn ended:\n{rows:#?}"
        );
    }

    #[test]
    fn a_call_added_to_a_run_takes_the_lid_with_it() {
        // The row index keeps a settled slot's measurement across frames —
        // that is what makes scrolling cheap — and a lid's slot used to ride
        // that cache: the run grew, the lid moved to the new last call, and
        // the old slot kept drawing the lid it had been measured with. The
        // screen showed `2 个工具` and then `3 个工具` over one run. The index
        // now never reuses a run's slots, so the lid must land on the new last
        // call alone — after a compose has already drawn the shorter run.
        let h = host();
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Group);
        let call = |id: &str| SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: id.into(),
                name: "read_file".into(),
                arguments: format!(r#"{{"file_path":"{id}.rs"}}"#),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        };
        let result = |id: &str| SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: id.into(),
            content: "ok".into(),
            is_error: false,
            images: Vec::new(),
        };
        h.absorb(&call("c1"));
        h.absorb(&result("c1"));
        h.absorb(&call("c2"));
        h.absorb(&result("c2"));
        let _ = h.compose((80, 40));

        // The run grows by one call — the exact mutation the reuse missed.
        h.absorb(&call("c3"));
        h.absorb(&result("c3"));

        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        let lids: Vec<&String> = rows.iter().filter(|r| r.contains("个工具")).collect();
        assert_eq!(
            lids.len(),
            1,
            "the old lid outlived the run it stood for:\n{rows:#?}"
        );
        assert!(
            lids[0].contains("3 个工具"),
            "the lid is not the run's new shape: {rows:?}"
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
        // Tool calls open by default now; ask for the merge lid explicitly, so
        // this test says which of the three modes it is about.
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Group);
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
            reasoning_blocks: Vec::new(),
            meta: None,
        };
        h.absorb(&call(1, "c1"));
        h.absorb(&SessionEvent::ToolResultLogged {
            turn: 1,
            round: 31,
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
    fn the_tool_output_modes_step_in_a_cycle_of_four() {
        // `Full → Head → Each → Group → Full`. The order runs from most detail
        // to least, so every press but the last answers "less of this" — and
        // the last has to come back round, or a person who overshot could not
        // get back without restarting the screen.
        let h = fed();
        let mode = |h: &Host| h.presentation.read().unwrap().tool_output();
        assert_eq!(mode(&h), crate::host::ToolOutput::Full, "the default");
        h.presentation.write().unwrap().toggle("tool_call");
        assert_eq!(mode(&h), crate::host::ToolOutput::Head);
        h.presentation.write().unwrap().toggle("tool_call");
        assert_eq!(mode(&h), crate::host::ToolOutput::Each);
        h.presentation.write().unwrap().toggle("tool_call");
        assert_eq!(mode(&h), crate::host::ToolOutput::Group);
        h.presentation.write().unwrap().toggle("tool_call");
        assert_eq!(mode(&h), crate::host::ToolOutput::Full, "back to the start");
    }

    #[test]
    fn head_previews_a_long_call_and_clicks_walk_the_three_shapes() {
        // The Head mode keeps most of a long call on screen — the first and
        // last twenty rows with a muted fold note between — and the click
        // walks preview → full → folded → preview, so a reader who opened a
        // call can still get back to the summary a further click away.
        let h = host();
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Head);
        let call = SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"seq 1 60"}"#.into(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        };
        let result = SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: "c1".into(),
            content: (1..=60)
                .map(|n| format!("row {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
            images: Vec::new(),
        };
        h.absorb(&call);
        h.absorb(&result);

        let rows: Vec<String> = h
            .compose((80, 80))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows.iter()
                .any(|r| r.contains("已折叠") && r.contains("点击展开")),
            "the fold note is not on screen:\n{rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("row 60")),
            "the tail rows are not shown:\n{rows:#?}"
        );
        let clipped = rows.iter().filter(|r| r.contains("row ")).count();
        assert!(
            clipped < 60,
            "the call is drawn whole, not previewed: {clipped} rows"
        );

        // Click: preview → full. `row 40` — mid-body, folded away before —
        // appears.
        let id = {
            let stream = h.stream.read().unwrap();
            stream.slots()[0].block().id
        };
        h.toggle_block(id, "tool_call");
        let rows: Vec<String> = h
            .compose((80, 80))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("row 40")),
            "the call did not open whole on the click:\n{rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("已折叠")),
            "the fold note outlived the opened call:\n{rows:#?}"
        );

        // Click again: full → folded (one summary row).
        h.toggle_block(id, "tool_call");
        let rows: Vec<String> = h
            .compose((80, 80))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("seq 1 60")),
            "the summary row is gone:\n{rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("row 40")),
            "the call is still whole after the second click:\n{rows:#?}"
        );

        // Click a third time: folded → preview, the mode's default shape.
        h.toggle_block(id, "tool_call");
        let rows: Vec<String> = h
            .compose((80, 80))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("已折叠")),
            "the call did not come back as a preview:\n{rows:#?}"
        );
    }

    #[test]
    fn each_draws_one_row_a_call_and_merges_nothing() {
        // The mode that used to be unspellable. `Folded` meant both "one row
        // each" and "one lid for the run", so asking for the first got the
        // second. Two calls have to be two rows here, and no count anywhere.
        let h = fed();
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Each);
        let rows: Vec<String> = h
            .compose((80, 40))
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            !rows.iter().any(|r| r.contains("个工具")),
            "a lid over the run, in the mode that is about drawing each call:\n{rows:#?}"
        );
        for subject in ["ReadFile(a.rs)", "ReadFile(b.rs)"] {
            assert!(
                rows.iter().any(|r| r.contains(subject)),
                "`{subject}` is not on a row of its own:\n{rows:#?}"
            );
        }
    }

    #[test]
    fn a_lid_says_how_many_of_the_run_failed_not_only_the_last() {
        // The lid draws the *last* call's result and nothing else, so a run
        // whose first call failed and whose second succeeded would read as one
        // that never failed — and the red on a failed call is the one thing a
        // fold has to keep. Built by hand rather than taken from the corpus,
        // which fails the call it also shows last.
        let h = host();
        h.presentation
            .write()
            .unwrap()
            .set_tool_output(crate::host::ToolOutput::Group);
        let call = |id: &str| SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: id.into(),
                name: "read_file".into(),
                arguments: format!(r#"{{"file_path":"{id}.rs"}}"#),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        };
        let result = |id: &str, is_error: bool| SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: id.into(),
            content: if is_error { "boom" } else { "ok" }.into(),
            is_error,
            images: Vec::new(),
        };
        h.absorb(&call("c1"));
        h.absorb(&result("c1", true)); // fails
        h.absorb(&call("c2"));
        h.absorb(&result("c2", false)); // and the one the lid shows succeeds

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
            .position(|r| r.contains("已执行了 2 个工具"))
            .expect("the lid does not say how many calls there were");
        assert!(
            rows[count].contains("1 失败"),
            "the failure earlier in the run is not on the lid: {:?}",
            &rows[count..count + 3]
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

    /// A call is expanded while it runs — its command is worth watching — and
    /// collapses to one summary row the moment its result lands, the way the
    /// reference does it. The default (`Full`) view, no `ctrl-t` needed.
    #[test]
    fn a_call_is_open_while_running_and_folds_once_it_finishes() {
        let h = host();
        let call = SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: r#"{"file_path":"a.rs"}"#.into(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        };
        h.absorb(&call);
        // Running: expanded, so the result gutter (`⎿ 运行中`) is on its own row.
        let running = h.compose((64, 40)).rows();
        assert!(
            running.iter().any(|r| r.contains('⎿')),
            "a running call is expanded:\n{running:#?}"
        );

        // The result lands — the call collapses to a single summary row.
        h.absorb(&SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: "c1".into(),
            content: "fn main() {}".into(),
            is_error: false,
            images: Vec::new(),
        });
        let done = h.compose((64, 40)).rows();
        assert!(
            !done.iter().any(|r| r.contains('⎿')),
            "a finished call folds to one row:\n{done:#?}"
        );
        assert!(
            done.iter()
                .any(|r| r.contains("ReadFile(a.rs)") && r.contains("fn main")),
            "the summary carries the result:\n{done:#?}"
        );
    }

    /// A folded call the model explained draws TWO rows, and the scroll counts
    /// both.
    ///
    /// The pair of facts is the point. The painter and `stream_height` are
    /// different walks over the same slots, and the count was a constant `1` while
    /// every folded block was one row; the moment one draws two, a constant there
    /// is the ghost-row class of bug this file keeps paying for — the terminal
    /// writes a row the arithmetic never reserved, and what comes after lands on
    /// top of it.
    ///
    /// Asserted as a delta on the same host, so it cannot be satisfied by a
    /// fixture that changed size: the only difference between the two measurements
    /// is which call it holds.
    #[test]
    fn a_folded_call_that_states_its_reason_is_counted_at_two_rows() {
        let explained =
            || host_with_one_call(r#"{"file_path":"a.rs","intent":"checking what main does"}"#);
        let silent = || host_with_one_call(r#"{"file_path":"a.rs"}"#);
        let size = (80u16, 24u16);
        let height = |h: &Host| h.stream_height(size, &h.moment.read().unwrap().clone());

        // The control first: with no reason the call is one row, which is what it
        // was before any of this existed.
        let h = silent();
        let quiet = h.compose(size).rows();
        assert_eq!(
            quiet.iter().filter(|r| r.contains("ReadFile")).count(),
            1,
            "an unexplained call still folds to one row:\n{quiet:#?}"
        );

        let h = explained();
        // The stream region, not `rows()`: `rows()` is the whole frame, and the
        // status bar and the input box are not part of the call.
        let frame = h.compose(size);
        let written: Vec<String> = frame
            .part("stream")
            .expect("the conversation")
            .lines
            .iter()
            .map(|l| l.plain())
            .filter(|r| !r.trim().is_empty())
            .collect();
        assert_eq!(
            written.len(),
            2,
            "a folded call with a reason draws two rows: {written:#?}"
        );
        assert!(
            written[0].contains("checking what main does"),
            "the reason is the first: {written:#?}"
        );
        assert!(
            written[1].contains("ReadFile(a.rs)"),
            "the call itself is the second: {written:#?}"
        );

        // And the sum agrees with the picture. Asserted as the DIFFERENCE the
        // reason makes, not as an absolute row count: an absolute number here
        // would be a second copy of the block's height, and it would keep passing
        // if the reason stopped being counted and the fixture were nudged.
        //
        // A row the count misses is a row out of reach at the bottom of the
        // scroll — the thing this file's `stream_height` notes go on about.
        let quiet = silent();
        assert_eq!(
            height(&h) - height(&quiet),
            1,
            "the reason adds a row to the picture but not to the sum: {} explained \
             vs {} silent",
            height(&h),
            height(&quiet)
        );
    }

    /// A host holding exactly one finished tool call, folded, with the given
    /// arguments.
    fn host_with_one_call(arguments: &str) -> Host {
        let h = host();
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: arguments.into(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        });
        h.absorb(&SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: "c1".into(),
            content: "fn main() {}".into(),
            is_error: false,
            images: Vec::new(),
        });
        h
    }

    /// `auto_folded` is the *default* layer under the hand fold, so it survives a
    /// mode cycle. A call the reader folds by hand while it runs must therefore
    /// come back to its one-row default — not spring open — once a full
    /// `Full → Head → Each → Group → Full` cycle has cleared the hand state. If
    /// `fold_finished` skipped hand-touched calls the finished call would be in
    /// neither `by_block` nor `auto_folded` after the cycle and draw expanded,
    /// breaking the "a full cycle comes back to the same screen" invariant.
    #[test]
    fn a_hand_folded_finished_call_keeps_its_default_after_a_mode_cycle() {
        let mut p = Presentation::default_folds();
        let id = BlockId(7);

        // Running: the reader folds it by hand, then it finishes.
        p.set_block(id, true);
        p.fold_finished(id);
        assert!(
            p.is_block_folded(id, "tool_call"),
            "the hand fold folds it while it stands"
        );

        // A full cycle of the four tool-output modes, ending back at the default
        // `Full`. The first step's `set_tool_output` clears `by_block`.
        for _ in 0..4 {
            p.toggle("tool_call");
        }
        assert_eq!(
            p.tool_output(),
            ToolOutput::Full,
            "back at the default view"
        );
        assert!(
            p.is_block_folded(id, "tool_call"),
            "the finished call falls back to its one-row default, not expanded"
        );
    }

    #[test]
    fn the_users_message_opens_a_paragraph_the_answer_starts_under() {
        // What you asked is the row you scan for. With the reply's first row
        // directly under the bar, the bar reads as the opening line of the
        // answer — the same collapse as prose running into a `●` header.
        //
        // `now break it` is the case that pins both halves at once: the turn
        // above it ends in a closing summary (whose own margin is the row above
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
        // A window tall enough to hold the whole corpus. The assertion is that
        // four kinds of row line up *with each other*, so it can only be made
        // where all four are on screen at once — and the corpus is longer than
        // a real terminal, because it is every awkward shape a module might
        // fold. When the corpus outgrows this, the row that fell off the top is
        // the user's bar and its `.expect` says so; the fix is a taller window,
        // not a shorter corpus.
        let h = fed();
        // Finished calls now auto-collapse to one row; expand one by hand so the
        // expanded-gutter alignment this test is about is on screen.
        let call_id = {
            let stream = h.stream.read().expect("stream poisoned");
            stream
                .slots()
                .iter()
                .find_map(|s| s.block().content.as_tool_call().map(|_| s.block().id))
                .expect("a tool call in the corpus")
        };
        h.presentation
            .write()
            .expect("presentation poisoned")
            .set_block(call_id, false);
        let rows = stream_rows(&h, (64, 120));
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
        // A folded call opens with its status dot at the margin (the dot is
        // content, so the line's own leading whitespace is 0) — the work is not
        // set in, only its reply is.
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
            reasoning_blocks: Vec::new(),
            meta: None,
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
            reasoning_blocks: Vec::new(),
            meta: None,
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
            reasoning_blocks: Vec::new(),
            meta: None,
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
            h.stream_height((40, 24), &Moment::default()),
            drawn,
            "the sum says there are {} rows to read and the frame drew {drawn}: \
             the reply was measured at the full 40 cells instead of the 38 it \
             is set in by, and the rows that difference makes are the rows at \
             the top of it that cannot be scrolled to",
            h.stream_height((40, 24), &Moment::default()),
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
    fn the_closing_summary_has_a_row_of_air_on_both_sides() {
        // The summary is the one row that is *about* the transcript: pressed
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
        let summary = rows
            .iter()
            // The first clean turn's closing summary: its outcome rotates through
            // `DONE_LABELS`, and index 0 is `Done`. A left-aligned line now, no ─.
            .position(|r| r.contains("Done"))
            .expect("the first turn's closing summary");
        // The prose it closes, a blank, the summary, a blank, the next question.
        assert!(
            rows[summary - 2].contains("Fixed it") && rows[summary + 2].contains("now break it"),
            "the summary is not between the two turns: {:?}",
            &rows[summary - 2..=summary + 2]
        );
        assert!(
            rows[summary - 1].trim().is_empty(),
            "no blank between the prose and the summary: {:?}",
            &rows[summary - 2..=summary]
        );
        assert!(
            rows[summary + 1].trim().is_empty(),
            "no blank between the summary and the next question: {:?}",
            &rows[summary..=summary + 2]
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
    fn a_scrolled_up_reader_watches_the_live_line_leave_like_content() {
        // This used to assert that the line *vanished* the moment the reader
        // left the bottom — a module reading `moment.scroll` and withdrawing.
        // It rides the stream's tail now, so what happens is what happens to
        // any content: scrolling moves it up, and one screen takes it away.
        //
        // The half that must NOT change is the second one here: the field does
        // not move. That is the `tip`/`input` invariant, and it is why neither
        // of those is in `TAIL`.
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
        let size = (60, 12);
        let at_bottom = h.compose(size);
        let line = at_bottom
            .part("live")
            .expect("the line is up while the reader is at the bottom")
            .rect;
        let field = at_bottom.part("input").expect("the field").rect;
        assert_eq!(line.h, 2, "the words and their blank row");

        // One row back and it is *shorter*, not gone: it is still on screen,
        // just clipped at the bottom edge like any other content at the fold.
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(1);
        let one_back = h.compose(size);
        assert_eq!(
            one_back.part("live").expect("still up").rect.h,
            1,
            "reading one row back does not delete the line"
        );

        // Past its own height and it has scrolled away — because the content
        // moved, not because the line decided to withdraw.
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(2);
        let past = h.compose(size);
        assert!(
            past.part("live").is_none(),
            "scrolled past it, so it is off the screen"
        );

        // And the frame held still throughout, which is the point of keeping
        // these two out of the tail.
        assert_eq!(
            past.part("input").expect("the field").rect,
            field,
            "the box does not move for it"
        );
        assert_eq!(
            past.part("stream").expect("the conversation").rect.h,
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
            reasoning_blocks: Vec::new(),
            meta: None,
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
            h.stream_height(size, &m) - h.stream_rows(size, &m) as usize,
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
    fn a_tail_taller_than_the_pane_still_leaves_the_top_of_the_scroll_reachable() {
        // The other half of the same property, now that the tail is part of the
        // sum. The cap in `cap_tail` has to be the *same* number in
        // `stream_height` and in the geometry: if the sum counted tail rows the
        // pane never drew, the last rows of the conversation would sit exactly
        // that far above the top of the scroll — reachable by the number and
        // not by the wheel.
        //
        // `scrolling_stops_at_the_oldest_line_not_past_it` above states the
        // arithmetic; this is what it looks like when it is wrong. The tail is
        // deliberately taller than the pane so the cap is in force: with a
        // short tail the two walks agree whether or not either caps.
        let h = host_with_the_shipped_tail();
        // One row per fact, so the oldest line is easy to name.
        for i in 0..40 {
            h.absorb(&SessionEvent::AssistantMessage {
                turn: 1,
                round: i,
                text: format!("marker-{i}"),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            });
        }
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 40,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![atomcode_kernel::tool::ToolCall {
                id: "t1".into(),
                name: "todowrite".into(),
                arguments: serde_json::json!({
                    "todos": (0..40).map(|i| serde_json::json!({
                        "content": format!("item {i}"),
                        "status": if i == 0 { "in_progress" } else { "pending" },
                    })).collect::<Vec<_>>()
                })
                .to_string(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        });

        let size = (80, 16);
        let m = h.moment.read().unwrap().clone();
        let rows = h.stream_rows(size, &m) as usize;
        let uncapped: usize = h
            .tail_heights(size.0, &m)
            .iter()
            .map(|(_, h)| *h as usize)
            .sum();
        assert!(
            uncapped > rows,
            "the tail asks for {uncapped} rows of a {rows}-row pane: the cap is idle \
             and this judgement cannot see it"
        );

        let limit = h.scroll_limit(size, &m);
        h.moment.write().unwrap().scroll = crate::moment::ScrollPos(limit);
        let text = h.compose(size).rows().join("\n");
        assert!(
            text.contains("marker-0"),
            "scrolled all the way back and the oldest line is not on screen — the \
             scroll bound counts rows the frame never drew:\n{text}"
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
    fn the_newest_line_is_the_last_one_drawn() {
        // With more to read than fits, the pane is full and there is no slack
        // anywhere, so this is about the order the walk fills the rows in — the
        // *padding* question (which end the slack goes) is the judgement below,
        // and the two are separate on purpose: a pane that cannot fill itself is
        // the only place they could be confused.
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
        // The corpus's turn 2 ends on an unanswered question and a self-cancel;
        // the cancel now draws no separator (it closes on the composer), so the
        // last line on screen is that turn's refused question card.
        assert!(
            last.plain().contains("拒绝"),
            "the newest line is the last one on screen: {:?}",
            last.plain()
        );
    }

    #[test]
    fn a_conversation_shorter_than_its_pane_starts_at_the_top_of_it() {
        // The padding goes **below** the content. It did not, and nothing said
        // so: a session's opening block sat against the composer with the blank
        // rows above it, which reads as a screen ending rather than one that has
        // just started — and the whole suite stayed green through it, because
        // every judgement here was about *what* was drawn and none about where
        // the slack went.
        let h = host();
        h.absorb(&SessionEvent::AssistantMessage {
            turn: 1,
            round: 0,
            text: "the one thing said".into(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            reasoning_blocks: Vec::new(),
            meta: None,
        });
        let frame = h.compose((80, 24));
        let stream = frame.part("stream").expect("the conversation");
        assert!(
            stream.rect.h > 4,
            "the fixture has to leave room to spare, or there is no slack to \\
             place and this judgement is about nothing: {:?}",
            stream.rect
        );

        let rows: Vec<String> = stream.lines.iter().map(|l| l.plain()).collect();
        let first = rows
            .iter()
            .position(|l| !l.trim().is_empty())
            .expect("something was said");
        assert_eq!(
            first, 0,
            "the conversation starts at the top of its pane, not against the \\
             composer: {rows:?}"
        );
        assert!(
            rows[0].contains("the one thing said"),
            "and it is the thing that was said: {rows:?}"
        );
        assert!(
            rows.last().is_some_and(|l| l.trim().is_empty()),
            "with the room left over underneath it, where the next line will \\
             land: {rows:?}"
        );

        // The control, and the half that keeps the change from being read
        // backwards: with more than a screenful there is no slack at all, so
        // moving the padding cannot have moved anything. Without this, "the
        // padding moved" and "the scroll was reversed" look the same from the
        // short case above.
        let full = fed();
        let frame = full.compose((80, 24));
        let stream = frame.part("stream").expect("the conversation");
        let trailing = stream
            .lines
            .iter()
            .rev()
            .take_while(|l| l.plain().trim().is_empty())
            .count();
        assert_eq!(
            trailing,
            0,
            "a pane with more to show than fits has no slack to place, so the \\
             newest row is the last row: {:?}",
            stream.lines.iter().map(|l| l.plain()).collect::<Vec<_>>()
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
            crate::menu::Item::new("help", "help").about("看命令"),
            crate::menu::Item::new("compact", "compact").about("压缩上下文"),
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
        // is filled to the rect with a panel background, so what it covers is
        // covered. A row of text spans that stopped at the last word would let
        // the conversation show through on the right.
        //
        // Two backgrounds are legitimate and no third one is: the plain panel,
        // and the one step brighter patch that says "this is the row a return
        // would take". The margin row at the foot is the plain one.
        let plain = Some(crate::frame::Color::role(crate::theme::Role::PanelBg));
        let lit = Some(crate::frame::Color::role(crate::theme::Role::PanelSelBg));
        for (i, line) in drawn.iter().enumerate() {
            assert_eq!(
                line.width(),
                menu.rect.w as usize,
                "menu row {i} does not fill the panel: {line:?}"
            );
            let want = if i == 0 { lit } else { plain };
            let bg = menu
                .lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.style.bg).collect::<Vec<_>>())
                .nth(i)
                .unwrap();
            assert!(
                line.spans.iter().all(|s| s.style.bg == want),
                "menu row {i} has cells with the wrong background ({bg:?}): {line:?}"
            );
        }

        h.set_menu(Vec::new());
        assert!(
            h.compose(size).part("menu").is_none(),
            "closing the menu takes its part away"
        );
    }

    /// A menu of `n` commands, as the composer's row would build it.
    fn menu_of(n: usize) -> Vec<crate::menu::Item> {
        (0..n)
            .map(|i| crate::menu::Item::new(format!("cmd{i}"), format!("cmd{i}")).about("does it"))
            .collect()
    }

    #[test]
    fn the_slash_menu_opens_with_its_first_row_lit() {
        // The requirement, at the level the host owns it: whatever narrowed the
        // list, the picture that comes out has row 0 highlighted. A cursor
        // carried across a narrowing would be pointing at whatever happens to
        // be in that slot now.
        let h = fed();
        let size = (80, 24);
        h.set_menu(menu_of(3));
        let frame = h.compose(size);
        let menu = frame.part("menu").expect("the menu is on screen");
        let lit = bright_rows(&menu.lines);
        assert_eq!(
            lit,
            vec![0],
            "the first row is the one that should be lit: {:?}",
            menu.lines.iter().map(|l| l.plain()).collect::<Vec<_>>()
        );

        // Narrowing recomputes the list, and the cursor starts over: the row
        // that was lit is not the same command once the list has changed.
        h.set_menu(menu_of(2));
        let menu = h.compose(size).part("menu").expect("still open").clone();
        assert_eq!(bright_rows(&menu.lines), vec![0]);
    }

    /// Which rows of a drawn panel are the brighter one.
    fn bright_rows(lines: &[Line]) -> Vec<usize> {
        let bright = Some(crate::frame::Color::role(crate::theme::Role::PanelSelBg));
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.spans.first().is_some_and(|s| s.style.bg == bright))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn moving_the_menu_cursor_moves_the_highlight_and_not_the_panel() {
        // Up/down walk the list; the panel stays where it grew out of. This is
        // the difference between a list and a thing that slides.
        let h = fed();
        let size = (80, 24);
        h.set_menu(menu_of(4));
        let before = h.compose(size).part("menu").expect("open").rect;

        assert!(h.menu_move_by(1), "there is a row below the first");
        let menu = h.compose(size).part("menu").expect("open").clone();
        assert_eq!(bright_rows(&menu.lines), vec![1]);
        assert_eq!(menu.rect, before, "the panel moved with the cursor");

        assert!(h.menu_move_by(-1));
        let menu = h.compose(size).part("menu").expect("open").clone();
        assert_eq!(bright_rows(&menu.lines), vec![0]);
        assert!(!h.menu_move_by(-1), "and there is nothing above the first");
    }

    #[test]
    fn a_long_list_is_a_window_that_follows_the_cursor_rather_than_a_cut_off_one() {
        // The bug this fixes: the panel is capped at ten rows, and the list was
        // drawn from the top, so command eleven was unreachable — visible
        // nowhere and selectable nowhere. The window scrolls with the cursor, so
        // the cap costs no reachability.
        let h = fed();
        let size = (80, 24);
        h.set_menu(menu_of(15));
        let menu = h.compose(size).part("menu").expect("open").clone();
        // The last row of the panel is the margin, so the list has ten rows.
        assert_eq!(
            menu.lines.len(),
            11,
            "the panel is the window plus its margin"
        );

        for _ in 0..14 {
            assert!(h.menu_move_by(1));
        }
        let menu = h.compose(size).part("menu").expect("open").clone();
        let drawn: Vec<String> = menu.lines.iter().map(|l| l.plain()).collect();
        assert!(
            drawn.iter().any(|l| l.contains("/cmd14")),
            "the last command is on screen: {drawn:?}"
        );
        assert!(
            !drawn.iter().any(|l| l.contains("/cmd0 ")),
            "and the window moved off the top: {drawn:?}"
        );
        assert_eq!(
            bright_rows(&menu.lines),
            vec![9],
            "the lit row is the last of the window"
        );
    }

    #[test]
    fn a_press_on_the_slash_menu_hands_back_the_row_it_was_drawn_on() {
        // Answered from the frame that was painted, not from a second layout:
        // the row a press lands on has to be the row that was under it.
        let h = fed();
        let size = (80, 24);
        h.set_menu(menu_of(4));
        let menu = h.compose(size).part("menu").expect("open").clone();
        let rect = menu.rect;
        // The second row of the list — the margin is the last row of the panel.
        let y = rect.y + 1;
        assert_eq!(
            h.menu_click(rect.x + 3, y).as_deref(),
            Some("cmd1"),
            "the press picked the row it was drawn on"
        );
        // And it is the highlight that moved, not just the return value.
        let menu = h.compose(size).part("menu").expect("open").clone();
        assert_eq!(bright_rows(&menu.lines), vec![1]);
    }

    #[test]
    fn a_press_beside_the_slash_menu_is_not_its_business() {
        let h = fed();
        let size = (80, 24);
        h.set_menu(menu_of(4));
        h.compose(size);
        assert_eq!(h.menu_click(0, 0), None, "the corner is not a menu row");
        assert_eq!(
            h.menu_selected().as_deref(),
            Some("cmd0"),
            "and nothing moved"
        );
    }

    #[test]
    fn the_pointer_lights_the_row_it_is_over_on_the_slash_menu() {
        let h = fed();
        let size = (80, 24);
        h.set_menu(menu_of(4));
        let menu = h.compose(size).part("menu").expect("open").clone();
        let rect = menu.rect;
        assert!(
            h.menu_hover(rect.x + 3, rect.y + 2),
            "the third row is a row the pointer moved onto"
        );
        assert_eq!(h.menu_selected().as_deref(), Some("cmd2"));
        assert!(
            !h.menu_hover(rect.x + 3, rect.y + 2),
            "a move inside the row it is already on is not news"
        );
        assert!(
            !h.menu_hover(0, 0),
            "and a move off the panel is not either"
        );
        assert_eq!(h.menu_selected().as_deref(), Some("cmd2"));
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
            reasoning_blocks: Vec::new(),
            meta: None,
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
            reasoning_blocks: Vec::new(),
            meta: None,
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
