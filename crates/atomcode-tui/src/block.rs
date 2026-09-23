//! The irreversible stream: blocks, their one-way lifecycle, and the only
//! handle that can write to it.
//!
//! What a person sees only ever grows. A block opens `Live`, may be amended
//! while it is, and settles exactly once. After that its **content** is frozen
//! — the type system says so, not a comment: there is no way to obtain
//! `&mut` to a settled block from any public API.
//!
//! **Content is frozen; presentation is not.** Folding a block, hiding it, or
//! rewrapping it at a new width are all the same content shown differently, and
//! `content_hash` is the instrument that tells the two apart. Presentation
//! lives in the host, keyed by [`BlockId`] — not on the block — so "folding
//! does not change content" is structural rather than a matter of discipline.
//!
//! Ordering is by **emission**, not by completion. Parallel tool calls leave
//! several blocks `Live` at once, and the third may settle after the fifth;
//! their positions never move. See `docs/adr/0004`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::frame::{Line, Style};

/// Stable identity for a block, for presentation and for folding targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u64);

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "b{}", self.0)
    }
}

/// Where a block sits in the conversation.
///
/// `turn` and `step` are **coordinates on blocks**, not blocks of their own:
/// every logged fact already carries them, so grouping and folding by turn need
/// no span blocks and no open/close pairing. See `docs/adr/0005`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Coord {
    pub turn: u64,
    pub step: u32,
}

impl Coord {
    pub const fn new(turn: u64, step: u32) -> Self {
        Self { turn, step }
    }
}

/// A content fingerprint. The instrument for "settled content never changes".
///
/// It covers the *semantic* content, never the rendered bytes — so rewrapping
/// at a new width, folding, or a theme change must leave it identical, while
/// any change to what the block says must not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContentHash(pub u64);

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Compute a hash from anything that can be written as bytes. Implementations
/// use this so they do not each invent a hashing scheme.
pub fn hash_of(parts: &[&str]) -> ContentHash {
    // FNV-1a: stable across runs and platforms, which matters because the hash
    // appears in assertions. `DefaultHasher` promises neither.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    ContentHash(h)
}

/// The part of the terminal's capabilities a block may see: **only the bits that
/// decide shape or existence.**
///
/// No `palette`. Colour is not a block's to decide — a block writes
/// `Color::Role(Role)` and [`crate::ansi::encode_with`] resolves it on the way
/// out, which is what `frame.rs:143-149` says and why: a block has no idea
/// whether the terminal is light or dark, and threading that answer through
/// every `lines` would mean every one of them could get it wrong. There is a
/// second, sharper reason here: `Palette` is *measured* by `measure_palette()`
/// and therefore changes with the terminal's theme, so a cache key carrying it
/// would mean **one theme change invalidates every settled block's render**.
///
/// When a bit is added to [`crate::caps::Caps`], ask whether it decides shape:
/// yes, add it here (and to the caches, which key on this); no, leave it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShapeCaps {
    pub unicode: bool,
    pub colors: crate::caps::Colors,
    /// Whether a cell's background colour paints.
    ///
    /// A *shape* bit, not a colour one, which is why it belongs here: the one
    /// thing a block does differently when it is false is draw **less** — half of
    /// a two-pixel-per-cell glyph is exactly the part that would be missing. See
    /// [`crate::caps::Caps::cell_background`].
    pub cell_background: bool,
}

impl ShapeCaps {
    /// The shape half of what the terminal was measured to be.
    pub fn of(caps: &crate::caps::Caps) -> Self {
        Self {
            unicode: caps.unicode,
            colors: caps.colors,
            cell_background: caps.cell_background,
        }
    }

    /// A decorative glyph as *this* terminal writes it.
    ///
    /// The same table as [`crate::caps::Caps::g`], see the docs there. A block
    /// that writes a decorative character must come through here rather than
    /// spell it out — `gates/tui-layers.sh` counts literal ones above the
    /// shield layer.
    pub fn g(&self, glyph: crate::caps::Glyph) -> &'static str {
        crate::caps::glyph(self.unicode, glyph)
    }
}

/// Everything rendering one block needs.
///
/// Two things, both enumerated: the width, and the capabilities that decide
/// shape. **Not a `Moment`** — that would drag in the composer's text, the
/// scroll position and the animation phase, and none of those may influence
/// what a block draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderCtx {
    pub width: u16,
    pub caps: ShapeCaps,
}

impl RenderCtx {
    /// Width only, with shape capabilities of a fully capable terminal.
    ///
    /// For tests and for the `Content` implementations that do not care about
    /// shape. The default is not a quiet downgrade: `Caps::default()` is what a
    /// modern terminal does (`caps.rs`).
    pub fn bare(width: u16) -> Self {
        Self {
            width,
            caps: ShapeCaps::of(&crate::caps::Caps::default()),
        }
    }
}

/// What a block says, as a semantic value rather than pre-rendered lines.
///
/// Pre-rendering would freeze the width, and then a resize could not rewrap
/// without changing the content — which the freeze rule forbids. Keeping it
/// semantic is what lets presentation stay mutable while content does not.
pub trait Content: Send + Sync + std::fmt::Debug {
    /// A short machine name for the kind of block this is (`"user"`,
    /// `"tool_call"`, …). Folding targets can name it.
    fn kind(&self) -> &'static str;

    /// The freeze instrument. Must depend on what the block *says* and on
    /// nothing else — not on width, theme, fold state, or wall-clock.
    fn content_hash(&self) -> ContentHash;

    /// Render at a width, on a terminal with these capabilities. Called every
    /// frame; must be pure.
    ///
    /// The capabilities are here for one thing only: a block whose *existence
    /// or shape* depends on what the terminal can draw. Glyphs that merely need
    /// downgrading do not need this — [`crate::ansi::write_line`] swaps those on
    /// the way out, one column in and one column out, so alignment survives.
    /// See `docs/adr/0025`.
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line>;

    /// The text this block is growing, when it can only ever grow.
    ///
    /// A streaming answer is one block whose text is appended to and never
    /// edited, and it is re-rendered every frame — so the honest way to make a
    /// frame cost the *new* text rather than the whole answer is to know that
    /// the text only extends. `None` for every other block: the cache this
    /// feeds is only correct for that shape, and a block that edits itself would
    /// silently keep stale lines.
    fn growing_text(&self) -> Option<&str> {
        None
    }

    /// Refuse to be folded.
    ///
    /// For the rare block whose whole point is that it happened — a skill being
    /// loaded changes how the agent behaves, and a one-line summary of that is
    /// a one-line summary of the most important thing on the screen.
    fn always_open(&self) -> bool {
        false
    }

    /// The `[Image #N]` attachment numbers this block shows, in order — empty
    /// for almost everything. A front end turns a click on a block that carries
    /// a picture into "open image N", the same gesture the composer supports for
    /// a marker still being typed. Only a block that renders user-typed text
    /// (where a `[Image #N]` marker can appear) overrides this.
    fn image_markers(&self) -> Vec<usize> {
        Vec::new()
    }

    /// One line standing in for the whole block when it is folded.
    fn summary(&self, ctx: &RenderCtx) -> Line {
        self.lines(ctx).into_iter().next().unwrap_or_default()
    }

    /// The block's folded form, as rows.
    ///
    /// One row for every block but a tool call: a call the model explained keeps
    /// BOTH of its rows when it folds, so the folded screen says the same two
    /// things the open one does and differs only by the result it left out —
    /// see `ToolCallBlock::summary_lines`.
    ///
    /// The default derives from [`summary`](Self::summary) rather than from
    /// `lines`, so a block that folds to something other than its first rendered
    /// row keeps doing so; overriding `summary` alone is therefore complete for
    /// every block that folds to one row. This is the accessor a folder should
    /// ask, and the row count and the picture must come from the same one.
    fn summary_lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        vec![self.summary(ctx)]
    }

    /// This block as a tool call, when it is one.
    ///
    /// The screen draws a run of folded calls as a single lid — `● 4 个工具`
    /// followed by the last command and its result — and drawing that is the
    /// host's job, not the block's. So the host needs the call itself rather
    /// than only its kind. A named accessor instead of a downcast: one case,
    /// stated, and impossible to get wrong at the call site.
    fn as_tool_call(&self) -> Option<&crate::content::ToolCallBlock> {
        None
    }
}

/// One entry in the stream.
#[derive(Debug)]
pub struct Block {
    pub id: BlockId,
    pub at: Coord,
    /// Which row produced it. A producer can be unloaded; its blocks stay.
    pub producer: &'static str,
    pub content: Arc<dyn Content>,
}

impl Block {
    pub fn kind(&self) -> &'static str {
        self.content.kind()
    }
    pub fn content_hash(&self) -> ContentHash {
        self.content.content_hash()
    }
}

/// A block's position in its one-way lifecycle.
#[derive(Debug)]
pub enum Slot {
    /// Still being produced. Its content may still change.
    Live(Block, LiveCache),
    /// Content frozen. Only presentation can change from here.
    Settled(Settled),
}

/// A live block's last render, so the next frame renders only what is new.
///
/// A live block is re-rendered every frame — it can have changed — and a
/// streaming answer only ever grows, so re-parsing all of it to add a word is
/// what makes a frame cost the length of the answer. Everything before the last
/// settled line boundary is fixed, so it is kept and only the tail is rendered
/// again. This dies with the block it describes, like [`Settled`]'s row count.
#[derive(Debug, Default)]
pub struct LiveCache(RwLock<Option<CachedRender>>);

/// How many times a live re-render resumed from the cache. Test-only: whether a
/// frame rendered one line or the whole answer cannot be seen in its output,
/// only in the work it did.
#[cfg(test)]
pub(crate) static LIVE_RESUMES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct CachedRender {
    /// What these rows were rendered for: the width, and the capabilities that
    /// decide shape.
    ///
    /// Both, because both are "a different question" rather than "a stale
    /// answer". **No palette** — colour is not decided here (see [`ShapeCaps`]),
    /// so a theme change does not walk into this branch.
    key: (u16, ShapeCaps),
    /// The source the lines were rendered from, whole. The prefix check against
    /// it is what makes reuse safe: text that was extended is text the kept
    /// lines still describe, and text that was edited is not.
    source: String,
    /// The rows this render produced, shared with whoever is drawing them.
    ///
    /// A handle rather than a copy: a frame wants a screenful of a block that
    /// may be hundreds of rows tall, so handing the rows over and letting the
    /// frame take the window it can show keeps a frame's cost the screen's
    /// rather than the answer's. The next render mutates these in place when
    /// this is the only handle left — which it is once the frame that took the
    /// last one has been painted and dropped — and copies when it is not.
    lines: Arc<Vec<Line>>,
    /// Byte offset in `source`, and line count in `lines`, up to which nothing
    /// can change. See [`crate::markdown::render_settled`].
    settled: usize,
    settled_lines: usize,
}

/// A settled block, and how many rows it was last measured to draw.
///
/// The count is kept here rather than in the painter because it is a fact about
/// the block: settling *is* the promise that content cannot change, so the
/// answer holds until the width does. A table in the host would need somebody
/// to remember to evict it; this dies with the block it describes.
#[derive(Debug)]
pub struct Settled {
    block: Arc<Block>,
    /// `(width, caps, rows)`. All three travel with the number, so a resize *and*
    /// a different terminal are each a different question rather than a stale
    /// answer.
    rows: RwLock<Option<(u16, ShapeCaps, usize)>>,
}

impl Settled {
    fn new(block: Arc<Block>) -> Self {
        Self {
            block,
            rows: RwLock::new(None),
        }
    }

    pub fn block(&self) -> &Block {
        &self.block
    }
}

impl Slot {
    pub fn block(&self) -> &Block {
        match self {
            Slot::Live(b, _) => b,
            Slot::Settled(s) => s.block(),
        }
    }
    pub fn is_live(&self) -> bool {
        matches!(self, Slot::Live(..))
    }
    pub fn is_settled(&self) -> bool {
        matches!(self, Slot::Settled(_))
    }

    /// How many rows this block draws at `width` — and the rows themselves
    /// when rendering them was the only way to find out.
    ///
    /// A settled block answers from its last measurement, because settling means
    /// its content cannot have changed; a live block is rendered every time,
    /// because it can have. The rows come back on a miss so that a caller which
    /// is about to draw them does not render the same block twice — the whole
    /// point being that the scroller and the painter ask one question and get
    /// one answer.
    ///
    /// They come back as a shared handle, not a copy. A block can be taller than
    /// the screen and a frame can only show a screenful of it, so the caller
    /// takes the window it has room for out of these rather than paying to
    /// duplicate rows it will drop. Held by the caller only for as long as it is
    /// drawing, which is what lets the next render extend them in place.
    pub fn rows_at(&self, ctx: &RenderCtx) -> (usize, Option<Arc<Vec<Line>>>) {
        match self {
            Slot::Live(b, cache) => {
                let Some(text) = b.content.growing_text() else {
                    let lines = b.content.lines(ctx);
                    return (lines.len(), Some(Arc::new(lines)));
                };
                let width = ctx.width;
                let mut cached = cache.0.write().expect("live cache poisoned");
                let base = Style::new();
                let rendered = match cached.take() {
                    // Reuse what is settled only when the same width rendered it
                    // and the text is an extension of what did. Anything else — a
                    // resize, an edit, a different block — renders the whole
                    // thing.
                    Some(mut c) if c.key == (width, ctx.caps) && text.starts_with(&c.source) => {
                        #[cfg(test)]
                        LIVE_RESUMES.fetch_add(1, Ordering::Relaxed);
                        // Everything before the last settled boundary is fixed;
                        // render from there, which is the part still in flux.
                        let lines = Arc::make_mut(&mut c.lines);
                        lines.truncate(c.settled_lines);
                        #[allow(
                            clippy::string_slice,
                            reason = "`c.settled` is a byte offset this render produced at a line boundary — the byte after a `\\n` — and `text` extends `c.source` byte for byte (the `starts_with` above), so the same offset is a char boundary in `text` too"
                        )]
                        let tail = crate::markdown::render_settled(&text[c.settled..], width, base);
                        lines.extend(tail.lines);
                        c.settled += tail.settled;
                        c.settled_lines += tail.settled_lines;
                        // Kept, not replaced: the same buffer, so a growing
                        // answer does not re-allocate its own source every frame.
                        c.source.clear();
                        c.source.push_str(text);
                        c
                    }
                    _ => {
                        let r = crate::markdown::render_settled(text, width, base);
                        CachedRender {
                            key: (width, ctx.caps),
                            source: text.to_string(),
                            lines: Arc::new(r.lines),
                            settled: r.settled,
                            settled_lines: r.settled_lines,
                        }
                    }
                };
                let lines = Arc::clone(&rendered.lines);
                *cached = Some(rendered);
                let n = lines.len();
                (n, Some(lines))
            }
            Slot::Settled(s) => {
                let mut measured = s.rows.write().expect("rows poisoned");
                if let Some((w, caps, n)) = *measured {
                    if w == ctx.width && caps == ctx.caps {
                        return (n, None);
                    }
                }
                let lines = s.block.content.lines(ctx);
                let rows = lines.len();
                *measured = Some((ctx.width, ctx.caps, rows));
                (rows, Some(Arc::new(lines)))
            }
        }
    }
}

/// The append-only sequence a person reads.
///
/// Positions are assigned on `open` and never move. Several slots may be `Live`
/// at once (parallel tool calls) and they may settle in any order.
#[derive(Debug, Default)]
pub struct Stream {
    slots: Vec<Slot>,
    next_id: AtomicU64,
}

impl Stream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything, in emission order.
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn get(&self, id: BlockId) -> Option<&Slot> {
        self.slots.iter().find(|s| s.block().id == id)
    }

    /// The settled prefix's fingerprints, in order. The value the freeze
    /// property is stated over.
    pub fn settled_hashes(&self) -> Vec<(BlockId, ContentHash)> {
        self.slots
            .iter()
            .filter(|s| s.is_settled())
            .map(|s| (s.block().id, s.block().content_hash()))
            .collect()
    }

    /// A writer for one producer. The **only** way to change a stream, and it
    /// cannot reach a settled block.
    pub fn writer(&mut self, producer: &'static str) -> StreamWriter<'_> {
        StreamWriter {
            stream: self,
            producer,
        }
    }
}

/// The capability a producer is given.
///
/// Deliberately narrow: open, amend the block *this producer* still has open,
/// settle it. There is no method that yields a settled block mutably, so
/// "settled content never changes" is enforced by reachability rather than by
/// review.
pub struct StreamWriter<'a> {
    stream: &'a mut Stream,
    producer: &'static str,
}

impl StreamWriter<'_> {
    /// Append a new block, `Live`.
    pub fn open(&mut self, at: Coord, content: Arc<dyn Content>) -> BlockId {
        let id = BlockId(self.stream.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        self.stream.slots.push(Slot::Live(
            Block {
                id,
                at,
                producer: self.producer,
                content,
            },
            LiveCache::default(),
        ));
        id
    }

    /// Append a block that is finished the moment it is made.
    pub fn emit(&mut self, at: Coord, content: Arc<dyn Content>) -> BlockId {
        let id = self.open(at, content);
        self.settle(id);
        id
    }

    /// Replace the content of a block that is still `Live` and belongs to this
    /// producer. `false` if it has settled, does not exist, or is someone
    /// else's — never a panic, and never a silent write to a frozen block.
    pub fn amend(&mut self, id: BlockId, content: Arc<dyn Content>) -> bool {
        for slot in self.stream.slots.iter_mut() {
            if let Slot::Live(b, _) = slot {
                if b.id == id && b.producer == self.producer {
                    b.content = content;
                    return true;
                }
            }
        }
        false
    }

    /// Freeze a block. Idempotent, and a no-op for someone else's.
    pub fn settle(&mut self, id: BlockId) -> bool {
        for slot in self.stream.slots.iter_mut() {
            if let Slot::Live(b, _) = slot {
                if b.id == id && b.producer == self.producer {
                    // `Live` is owned, so this moves rather than clones.
                    let placeholder = Slot::Settled(Settled::new(Arc::new(Block {
                        id,
                        at: b.at,
                        producer: b.producer,
                        content: b.content.clone(),
                    })));
                    *slot = placeholder;
                    return true;
                }
            }
        }
        false
    }

    /// Settle everything this producer still has open. For the end of a turn.
    pub fn settle_all(&mut self) {
        let live: Vec<BlockId> = self
            .stream
            .slots
            .iter()
            .filter_map(|s| match s {
                Slot::Live(b, _) if b.producer == self.producer => Some(b.id),
                _ => None,
            })
            .collect();
        for id in live {
            self.settle(id);
        }
    }

    /// The ids this producer still has open, oldest first.
    pub fn live(&self) -> Vec<BlockId> {
        self.stream
            .slots
            .iter()
            .filter_map(|s| match s {
                Slot::Live(b, _) if b.producer == self.producer => Some(b.id),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Text(&'static str);

    impl Content for Text {
        fn kind(&self) -> &'static str {
            "text"
        }
        fn content_hash(&self) -> ContentHash {
            hash_of(&[self.0])
        }
        fn lines(&self, _ctx: &RenderCtx) -> Vec<Line> {
            vec![Line::raw(self.0)]
        }
    }

    fn text(s: &'static str) -> Arc<dyn Content> {
        Arc::new(Text(s))
    }

    /// A block that draws two rows on a terminal that does Unicode, one otherwise.
    ///
    /// Deliberately a *real* capability-dependent block rather than a mock: a
    /// judgement about the cache key has to fail when the key does not carry the
    /// capability, and a block that drew the same either way would let it pass.
    #[derive(Debug)]
    struct TwoRowsWhenUnicode;

    impl Content for TwoRowsWhenUnicode {
        fn kind(&self) -> &'static str {
            "two_rows"
        }
        fn content_hash(&self) -> ContentHash {
            // Shape is not in the hash: same class as width, see `Content`.
            hash_of(&["two_rows"])
        }
        fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
            let n = if ctx.caps.unicode { 2 } else { 1 };
            (0..n).map(|_| Line::raw("x")).collect()
        }
    }

    fn shape(unicode: bool) -> ShapeCaps {
        ShapeCaps {
            unicode,
            colors: crate::caps::Colors::Ansi256,
            cell_background: true,
        }
    }

    #[test]
    fn a_change_of_terminal_capability_is_not_a_cache_hit() {
        let mut s = Stream::new();
        let mut w = s.writer("test");
        let id = w.emit(Coord::default(), Arc::new(TwoRowsWhenUnicode));
        w.settle_all();

        let unicode = RenderCtx {
            width: 40,
            caps: shape(true),
        };
        let ascii = RenderCtx {
            width: 40,
            caps: shape(false),
        };

        let slot = s.get(id).expect("the block");
        assert_eq!(slot.rows_at(&unicode).0, 2);
        assert_eq!(
            slot.rows_at(&ascii).0,
            1,
            "the same width with different capabilities must render again rather \
             than hit the cached row count — otherwise a terminal that changed \
             under us keeps the old answer, and it looks fine"
        );
        // Back again, so neither number was reached by accident.
        assert_eq!(slot.rows_at(&unicode).0, 2);
    }

    #[test]
    fn the_same_key_is_a_cache_hit_and_a_resize_is_not() {
        // The negative control for the judgement above. Without it, "render every
        // time" would satisfy that one too.
        let mut s = Stream::new();
        let mut w = s.writer("test");
        let id = w.emit(Coord::default(), Arc::new(TwoRowsWhenUnicode));
        w.settle_all();

        let ctx = RenderCtx {
            width: 40,
            caps: shape(true),
        };
        let slot = s.get(id).expect("the block");
        assert_eq!(slot.rows_at(&ctx).0, 2);
        assert_eq!(
            slot.rows_at(&ctx).1,
            None,
            "a settled block at the same width and the same capabilities must \
             answer from what it measured"
        );
        assert!(
            slot.rows_at(&RenderCtx { width: 41, ..ctx }).1.is_some(),
            "a resize is a different question and must render again"
        );
    }

    #[test]
    fn a_settled_block_cannot_be_amended() {
        let mut s = Stream::new();
        let mut w = s.writer("p");
        let id = w.open(Coord::new(1, 1), text("draft"));
        assert!(w.amend(id, text("better")), "live blocks take amendments");
        assert!(w.settle(id));
        assert!(
            !w.amend(id, text("sneaky")),
            "a settled block must refuse, not silently accept"
        );
        assert_eq!(
            s.get(id)
                .unwrap()
                .block()
                .content
                .lines(&crate::block::RenderCtx::bare(80))[0]
                .plain(),
            "better"
        );
    }

    #[test]
    fn another_producer_cannot_touch_my_block() {
        let mut s = Stream::new();
        let id = s.writer("mine").open(Coord::new(1, 1), text("a"));
        let mut theirs = s.writer("theirs");
        assert!(!theirs.amend(id, text("b")));
        assert!(!theirs.settle(id));
    }

    #[test]
    fn positions_follow_emission_not_settlement() {
        let mut s = Stream::new();
        let mut w = s.writer("p");
        let a = w.open(Coord::new(1, 1), text("first"));
        let b = w.open(Coord::new(1, 1), text("second"));
        let c = w.open(Coord::new(1, 1), text("third"));
        // Settle out of order, as parallel tools do.
        w.settle(c);
        w.settle(a);
        w.settle(b);
        let order: Vec<_> = s.slots().iter().map(|s| s.block().id).collect();
        assert_eq!(order, vec![a, b, c], "emission order is frozen");
    }

    #[test]
    fn several_blocks_are_live_at_once() {
        let mut s = Stream::new();
        let mut w = s.writer("p");
        w.open(Coord::new(1, 1), text("a"));
        w.open(Coord::new(1, 1), text("b"));
        assert_eq!(w.live().len(), 2, "parallel calls leave several open");
        w.settle_all();
        assert!(s.slots().iter().all(|s| s.is_settled()));
    }

    #[test]
    fn the_settled_prefix_only_grows() {
        let mut s = Stream::new();
        let mut w = s.writer("p");
        w.emit(Coord::new(1, 1), text("one"));
        let before = s.settled_hashes();
        let mut w = s.writer("p");
        w.emit(Coord::new(1, 2), text("two"));
        let after = s.settled_hashes();
        assert_eq!(&after[..before.len()], &before[..], "prefix is stable");
        assert_eq!(after.len(), before.len() + 1);
    }

    #[test]
    fn the_hash_is_stable_across_runs_not_just_within_one() {
        // A `DefaultHasher` would make this test pass and CI fail.
        assert_eq!(hash_of(&["hello"]), hash_of(&["hello"]));
        assert_ne!(hash_of(&["hello"]), hash_of(&["hell", "o"]));
    }

    // ---- the live render cache -------------------------------------------

    use crate::content::ModelSaid;
    use std::sync::atomic::Ordering as At;

    /// The resume count is process-global, so tests that read it must not run
    /// beside each other.
    static LIVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn live_alone() -> std::sync::MutexGuard<'static, ()> {
        LIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn grew(text: &str) -> Arc<dyn Content> {
        Arc::new(ModelSaid(text.to_string()))
    }

    fn resumes() -> u64 {
        LIVE_RESUMES.load(At::Relaxed)
    }

    #[test]
    fn a_growing_block_renders_only_the_new_tail() {
        let _alone = live_alone();
        let mut s = Stream::new();
        let id = {
            let mut w = s.writer("model");
            w.open(Coord::new(1, 1), grew("first line\n\n"))
        };

        // The first render has nothing to resume from, and is the whole text.
        LIVE_RESUMES.store(0, At::Relaxed);
        let (n1, l1) = s
            .get(id)
            .unwrap()
            .rows_at(&crate::block::RenderCtx::bare(30));
        assert_eq!(resumes(), 0, "nothing was rendered to resume from");
        assert_eq!(
            *l1.unwrap(),
            crate::markdown::render("first line\n\n", 30, Style::new())
        );

        // A word appended: the second render resumes, and still describes the
        // whole answer.
        {
            let mut w = s.writer("model");
            w.amend(id, grew("first line\n\nsecond paragraph, still going\n"));
        }
        LIVE_RESUMES.store(0, At::Relaxed);
        let (n2, l2) = s
            .get(id)
            .unwrap()
            .rows_at(&crate::block::RenderCtx::bare(30));
        assert_eq!(resumes(), 1, "the growing render resumed from the cache");
        assert_eq!(
            *l2.unwrap(),
            crate::markdown::render(
                "first line\n\nsecond paragraph, still going\n",
                30,
                Style::new()
            )
        );
        assert!(n2 > n1, "the block got taller");
    }

    #[test]
    fn text_that_was_not_extended_is_not_reused() {
        // The cache is safe only because a streaming answer is append-only. An
        // edit that is not an extension must fall back to a full render, or the
        // kept lines would describe something the block no longer says.
        let _alone = live_alone();
        let mut s = Stream::new();
        let id = {
            let mut w = s.writer("model");
            w.open(Coord::new(1, 1), grew("first answer\n"))
        };
        let _ = s
            .get(id)
            .unwrap()
            .rows_at(&crate::block::RenderCtx::bare(30));

        {
            let mut w = s.writer("model");
            w.amend(id, grew("a completely different answer\n"));
        }
        LIVE_RESUMES.store(0, At::Relaxed);
        let (_, lines) = s
            .get(id)
            .unwrap()
            .rows_at(&crate::block::RenderCtx::bare(30));
        assert_eq!(resumes(), 0, "an edit is not an append");
        assert_eq!(
            *lines.unwrap(),
            crate::markdown::render("a completely different answer\n", 30, Style::new())
        );
    }

    #[test]
    fn the_rows_a_frame_drew_are_extended_in_place_not_copied_out() {
        // The cost this exists for. A frame wants a screenful of a block that
        // may be hundreds of rows tall, so the cache hands the rows over instead
        // of copying them out — and the next render extends that same buffer.
        // Copying them out would put the whole answer back into every frame,
        // which is what the incremental render was for. Not visible in a frame's
        // output, so it is asserted on the render that followed.
        let _alone = live_alone();
        let mut s = Stream::new();
        let id = {
            let mut w = s.writer("model");
            w.open(Coord::new(1, 1), grew("first line\n\n"))
        };

        let first = s
            .get(id)
            .unwrap()
            .rows_at(&crate::block::RenderCtx::bare(30))
            .1
            .expect("a first render");
        let rows = first.len();
        let home = Arc::as_ptr(&first) as usize;
        // The frame that took these has been painted and dropped by now, which
        // is what leaves this the only handle on them.
        drop(first);

        {
            let mut w = s.writer("model");
            w.amend(id, grew("first line\n\nsecond paragraph, still going\n"));
        }
        let second = s
            .get(id)
            .unwrap()
            .rows_at(&crate::block::RenderCtx::bare(30))
            .1
            .expect("a second render");
        assert!(second.len() > rows, "the block got taller");
        assert_eq!(
            Arc::as_ptr(&second) as usize,
            home,
            "the rows came back in a new buffer: the frame's rows were copied \
             out of the cache, so the answer was re-materialised for the frame \
             instead of being extended in place"
        );
    }

    #[test]
    fn a_resize_starts_the_render_over() {
        let _alone = live_alone();
        let mut s = Stream::new();
        let id = {
            let mut w = s.writer("model");
            w.open(
                Coord::new(1, 1),
                grew("a line long enough to wrap differently\n"),
            )
        };
        let _ = s
            .get(id)
            .unwrap()
            .rows_at(&crate::block::RenderCtx::bare(30));

        LIVE_RESUMES.store(0, At::Relaxed);
        let (_, lines) = s
            .get(id)
            .unwrap()
            .rows_at(&crate::block::RenderCtx::bare(12));
        assert_eq!(resumes(), 0, "another width is another render");
        assert_eq!(
            *lines.unwrap(),
            crate::markdown::render("a line long enough to wrap differently\n", 12, Style::new())
        );
    }
}
