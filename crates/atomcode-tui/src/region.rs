//! The region tree: where each module goes.
//!
//! Layout is **data**, so it can be patched, differ per product variant, and be
//! rearranged at runtime by a key, a command or the model — see
//! `docs/adr/0007`. Assigning rects is pure geometry over this tree, which is
//! why it can be tested with no modules mounted at all.

use crate::frame::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// `a` above `b`.
    Vertical,
    /// `a` left of `b`.
    Horizontal,
}

/// How much of the parent the first child gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constraint {
    /// Exactly this many cells, clamped to what exists.
    Cells(u16),
    /// This percentage, rounded down.
    Percent(u8),
    /// Whatever is left after the other side takes its fixed size.
    Fill,
}

/// A placement tree. Leaves name a module; the host resolves the name against
/// the mounted rows *for a realm*, so the tree says where and the realm says
/// which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Region {
    /// The irreversible stream.
    Stream,
    /// A view module, by id.
    View(String),
    Split {
        dir: Dir,
        at: Constraint,
        a: Box<Region>,
        b: Box<Region>,
    },
    /// Overlaid, later on top. Exactly one may hold focus.
    Stack(Vec<Region>),
    /// Nothing. What a split collapses to when a module is not mounted.
    Empty,
}

impl Region {
    pub fn view(id: impl Into<String>) -> Region {
        Region::View(id.into())
    }

    pub fn split(dir: Dir, at: Constraint, a: Region, b: Region) -> Region {
        Region::Split {
            dir,
            at,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    /// Stream on top, `below` underneath, `below` taking `rows`.
    pub fn stream_over(below: Region, rows: u16) -> Region {
        Region::Split {
            dir: Dir::Vertical,
            at: Constraint::Fill,
            a: Box::new(Region::Stream),
            b: Box::new(below),
        }
        .with_second_size(rows)
    }

    fn with_second_size(self, rows: u16) -> Region {
        match self {
            Region::Split { dir, a, b, .. } => Region::Split {
                dir,
                at: Constraint::Cells(rows),
                // `Cells` sizes the *first* child, so swap and keep meaning.
                a: b,
                b: a,
            }
            .flipped(),
            other => other,
        }
    }

    fn flipped(self) -> Region {
        match self {
            Region::Split { dir, at, a, b } => Region::Split {
                dir,
                at,
                a: b,
                b: a,
            },
            other => other,
        }
    }

    /// Every module id this tree names, in tree order.
    pub fn modules(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(&mut |r| {
            if let Region::View(id) = r {
                out.push(id.clone());
            }
        });
        out
    }

    pub fn has_stream(&self) -> bool {
        let mut found = false;
        self.walk(&mut |r| {
            if matches!(r, Region::Stream) {
                found = true;
            }
        });
        found
    }

    fn walk(&self, f: &mut impl FnMut(&Region)) {
        f(self);
        match self {
            Region::Split { a, b, .. } => {
                a.walk(f);
                b.walk(f);
            }
            Region::Stack(children) => children.iter().for_each(|c| c.walk(f)),
            _ => {}
        }
    }

    /// Drop leaves naming modules that are not mounted, collapsing the splits
    /// they leave behind.
    ///
    /// A layout that mentions `findings` must still work where that row is not
    /// mounted — a variant's layout has to survive being used by another
    /// variant. Collapsing, not panicking, is what makes that true.
    pub fn prune(&self, mounted: &dyn Fn(&str) -> bool) -> Region {
        match self {
            Region::View(id) if !mounted(id) => Region::Empty,
            Region::Split { dir, at, a, b } => {
                let a = a.prune(mounted);
                let b = b.prune(mounted);
                match (&a, &b) {
                    (Region::Empty, Region::Empty) => Region::Empty,
                    (Region::Empty, _) => b,
                    (_, Region::Empty) => a,
                    _ => Region::Split {
                        dir: *dir,
                        at: *at,
                        a: Box::new(a),
                        b: Box::new(b),
                    },
                }
            }
            Region::Stack(children) => {
                let kept: Vec<_> = children
                    .iter()
                    .map(|c| c.prune(mounted))
                    .filter(|c| !matches!(c, Region::Empty))
                    .collect();
                match kept.len() {
                    0 => Region::Empty,
                    1 => kept.into_iter().next().unwrap(),
                    _ => Region::Stack(kept),
                }
            }
            other => other.clone(),
        }
    }

    /// Assign a rect to every leaf, giving every module one row.
    pub fn layout(&self, area: Rect) -> Vec<(Region, Rect)> {
        self.layout_with(area, &|_| 1)
    }

    /// Assign rects, asking `wants` how many rows each module would like.
    ///
    /// Arbitration lives here rather than in the modules: a module *requests* a
    /// height and the tree decides, so one module can never seize the screen —
    /// and `Fill` can leave exactly the right amount for what sits beside it,
    /// which pure geometry alone cannot know.
    pub fn layout_with(&self, area: Rect, wants: &dyn Fn(&str) -> u16) -> Vec<(Region, Rect)> {
        let mut out = Vec::new();
        self.lay(area, wants, &mut out);
        out
    }

    fn lay(&self, area: Rect, wants: &dyn Fn(&str) -> u16, out: &mut Vec<(Region, Rect)>) {
        if area.is_empty() {
            return;
        }
        match self {
            Region::Empty => {}
            Region::Stream | Region::View(_) => out.push((self.clone(), area)),
            Region::Stack(children) => children.iter().for_each(|c| c.lay(area, wants, out)),
            Region::Split { dir, at, a, b } => {
                let total = match dir {
                    Dir::Vertical => area.h,
                    Dir::Horizontal => area.w,
                };
                let first = match at {
                    Constraint::Cells(n) => (*n).min(total),
                    Constraint::Percent(p) => ((total as u32 * (*p).min(100) as u32) / 100) as u16,
                    // Leave the other side what it asked for, but never so much
                    // that this side vanishes: a module asking for more than the
                    // screen gets what there is, not everything.
                    Constraint::Fill => {
                        let other = b.wanted(*dir, wants).min(total.saturating_sub(1));
                        total.saturating_sub(other)
                    }
                };
                let (ra, rb) = match dir {
                    Dir::Vertical => area.split_v(first),
                    Dir::Horizontal => area.split_h(first),
                };
                a.lay(ra, wants, out);
                b.lay(rb, wants, out);
            }
        }
    }

    /// How much a subtree asks for when the other side takes `Fill`.
    fn wanted(&self, dir: Dir, wants: &dyn Fn(&str) -> u16) -> u16 {
        match self {
            Region::Empty => 0,
            Region::Stream => 1,
            Region::View(id) => wants(id).max(1),
            Region::Stack(c) => c.iter().map(|r| r.wanted(dir, wants)).max().unwrap_or(0),
            Region::Split { dir: d, at, a, b } => {
                let (sa, sb) = (a.wanted(dir, wants), b.wanted(dir, wants));
                if *d == dir {
                    match at {
                        Constraint::Cells(n) => n.saturating_add(sb),
                        _ => sa.saturating_add(sb),
                    }
                } else {
                    sa.max(sb)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[(Region, Rect)]) -> Vec<String> {
        v.iter()
            .map(|(r, _)| match r {
                Region::Stream => "stream".to_string(),
                Region::View(id) => id.clone(),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn a_fixed_split_gives_exactly_what_it_asks_for() {
        let tree = Region::split(
            Dir::Vertical,
            Constraint::Cells(3),
            Region::view("top"),
            Region::view("bottom"),
        );
        let out = tree.layout(Rect::sized(20, 10));
        assert_eq!(ids(&out), vec!["top", "bottom"]);
        assert_eq!(out[0].1, Rect::new(0, 0, 20, 3));
        assert_eq!(out[1].1, Rect::new(0, 3, 20, 7));
    }

    #[test]
    fn fill_leaves_room_for_what_is_below_it() {
        // The shape the TUI actually uses: stream takes what is left.
        let tree = Region::split(
            Dir::Vertical,
            Constraint::Fill,
            Region::Stream,
            Region::split(
                Dir::Vertical,
                Constraint::Cells(1),
                Region::view("status"),
                Region::view("input"),
            ),
        );
        let out = tree.layout(Rect::sized(40, 12));
        assert_eq!(ids(&out), vec!["stream", "status", "input"]);
        assert_eq!(out[0].1.h + out[1].1.h + out[2].1.h, 12, "no rows lost");
        assert_eq!(out[1].1.h, 1);
    }

    #[test]
    fn an_unmounted_module_collapses_instead_of_panicking() {
        let tree = Region::split(
            Dir::Horizontal,
            Constraint::Percent(70),
            Region::Stream,
            Region::view("findings"),
        );
        let pruned = tree.prune(&|id| id != "findings");
        assert_eq!(
            pruned,
            Region::Stream,
            "the split collapses to what is left"
        );
        let out = pruned.layout(Rect::sized(30, 5));
        assert_eq!(out[0].1, Rect::sized(30, 5), "the survivor takes the space");
    }

    #[test]
    fn layout_never_panics_at_any_size() {
        let tree = Region::split(
            Dir::Vertical,
            Constraint::Fill,
            Region::Stream,
            Region::split(
                Dir::Horizontal,
                Constraint::Percent(30),
                Region::view("a"),
                Region::Stack(vec![Region::view("b"), Region::view("c")]),
            ),
        );
        for w in 0..40u16 {
            for h in 0..20u16 {
                let out = tree.layout(Rect::sized(w, h));
                for (_, r) in out {
                    assert!(r.right() <= w && r.bottom() <= h, "{r:?} outside {w}×{h}");
                }
            }
        }
    }

    #[test]
    fn a_stack_puts_every_child_in_the_same_rect() {
        let tree = Region::Stack(vec![Region::view("under"), Region::view("over")]);
        let out = tree.layout(Rect::sized(10, 4));
        assert_eq!(ids(&out), vec!["under", "over"], "later is on top");
        assert_eq!(out[0].1, out[1].1);
    }
}
