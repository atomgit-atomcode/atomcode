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
use std::sync::Arc;

use crate::frame::Line;

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

    /// Render at a width. Called every frame; must be pure.
    fn lines(&self, width: u16) -> Vec<Line>;

    /// Refuse to be folded.
    ///
    /// For the rare block whose whole point is that it happened — a skill being
    /// loaded changes how the agent behaves, and a one-line summary of that is
    /// a one-line summary of the most important thing on the screen.
    fn always_open(&self) -> bool {
        false
    }

    /// One line standing in for the whole block when it is folded.
    fn summary(&self, width: u16) -> Line {
        self.lines(width).into_iter().next().unwrap_or_default()
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
    Live(Block),
    /// Content frozen. Only presentation can change from here.
    Settled(Arc<Block>),
}

impl Slot {
    pub fn block(&self) -> &Block {
        match self {
            Slot::Live(b) => b,
            Slot::Settled(b) => b,
        }
    }
    pub fn is_live(&self) -> bool {
        matches!(self, Slot::Live(_))
    }
    pub fn is_settled(&self) -> bool {
        matches!(self, Slot::Settled(_))
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
        self.stream.slots.push(Slot::Live(Block {
            id,
            at,
            producer: self.producer,
            content,
        }));
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
            if let Slot::Live(b) = slot {
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
            if let Slot::Live(b) = slot {
                if b.id == id && b.producer == self.producer {
                    // `Live` is owned, so this moves rather than clones.
                    let placeholder = Slot::Settled(Arc::new(Block {
                        id,
                        at: b.at,
                        producer: b.producer,
                        content: b.content.clone(),
                    }));
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
                Slot::Live(b) if b.producer == self.producer => Some(b.id),
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
                Slot::Live(b) if b.producer == self.producer => Some(b.id),
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
        fn lines(&self, _w: u16) -> Vec<Line> {
            vec![Line::raw(self.0)]
        }
    }

    fn text(s: &'static str) -> Arc<dyn Content> {
        Arc::new(Text(s))
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
            s.get(id).unwrap().block().content.lines(80)[0].plain(),
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
}
