//! Changing the screen's shape while it runs.
//!
//! Three ways in — a key, a command, the model — and they must produce the
//! **same value**, or there are three implementations of layout editing that
//! will drift. So the vocabulary is one enum, and everything else is a
//! translator into it.
//!
//! Changes are ops rather than whole-tree replacement for three reasons: undo
//! needs an inverse, a failure needs a reason the model can act on, and a model
//! asked to emit a whole tree gets it wrong far more often than one asked to
//! name an intent. See `docs/adr/0007`.

use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::el::Item;
use crate::region::{Constraint, Dir, Region};

/// Which part of the screen an op is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// By module id — how a person and a model both name things.
    Module(String),
    /// The conversation itself.
    Stream,
}

/// Where a module goes when it is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

impl Side {
    fn dir(self) -> Dir {
        match self {
            Side::Top | Side::Bottom => Dir::Vertical,
            Side::Left | Side::Right => Dir::Horizontal,
        }
    }
    fn first(self) -> bool {
        matches!(self, Side::Top | Side::Left)
    }
}

/// One change to the screen's shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LayoutOp {
    /// Put a module on screen, on this side of everything else.
    Show {
        module: String,
        side: Side,
        /// Rows or columns. `None` asks for a sensible default.
        size: Option<u16>,
    },
    /// Take a module off screen. Its state is untouched.
    Hide { module: String },
    /// Swap two things' places.
    Swap { a: Target, b: Target },
    /// Resize whatever holds `target`.
    Resize { target: Target, size: u16 },
    /// A named arrangement.
    Preset { name: String },
    /// Put back what the last op changed.
    Undo,
}

/// Why an op could not run — in terms the model can act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutError {
    NoSuchModule {
        name: String,
        available: Vec<String>,
    },
    NotOnScreen(String),
    AlreadyOnScreen(String),
    NoSuchPreset {
        name: String,
        available: Vec<String>,
    },
    /// One module named both as a tail id and as a leaf of its own, which would
    /// draw it twice. See [`El::named_twice`](crate::el::El::named_twice).
    NamedTwice(String),
    /// The target is a module riding the stream's tail, which has no box of its
    /// own to move or resize. Move it out first (`/hide`, then `/show`) and aim
    /// at it there.
    TailIsNotATarget(String),
    NothingToUndo,
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::NoSuchModule { name, available } => {
                write!(f, "没有叫 `{name}` 的模块;现有的是:{}", available.join(" "))
            }
            LayoutError::NotOnScreen(m) => write!(f, "`{m}` 本来就不在屏幕上"),
            LayoutError::AlreadyOnScreen(m) => write!(f, "`{m}` 已经在屏幕上了"),
            LayoutError::NoSuchPreset { name, available } => {
                write!(f, "没有叫 `{name}` 的布局;有的是:{}", available.join(" "))
            }
            LayoutError::NamedTwice(m) => {
                write!(f, "`{m}` 被写了两遍:既在流尾部、又是独立面板,会被画两次")
            }
            LayoutError::TailIsNotATarget(m) => write!(
                f,
                "`{m}` 骑在对话的尾部,没有自己的位置可换或可改;先 `/hide {m}` 再 `/show` 它"
            ),
            LayoutError::NothingToUndo => write!(f, "没有可撤销的布局改动"),
        }
    }
}

/// The named arrangements this build ships.
pub fn presets() -> Vec<(&'static str, &'static str)> {
    vec![
        ("default", "状态栏 · 对话 · 实时行 · 输入"),
        ("focus", "状态栏收起,只剩对话、实时行和输入"),
        ("wide", "对话在左,面板在右"),
    ]
}

/// The tree a named preset resolves to, for tests that need to inspect one.
///
/// The op path reaches `preset` through `Layout::apply`; this exists so a test
/// can hold the tree itself — a shipped arrangement is a tree that never passes
/// that check, and what it names is worth asserting directly.
#[cfg(test)]
pub(crate) fn preset_for_test(name: &str) -> Option<Region> {
    preset(name)
}

fn preset(name: &str) -> Option<Region> {
    let base = crate::host::default_layout();
    // Whatever arrangement is chosen, the input box keeps the live line above
    // it: it belongs to the composer (`crate::host::composer`), not to a panel
    // that happens to be on screen — a screen a person picked for its shape
    // should not lose the one row that says whether the turn is still moving.
    let composer = crate::host::composer();
    match name {
        "default" => Some(base),
        "focus" => Some(Region::split(
            Dir::Vertical,
            Constraint::Fill,
            crate::host::scroll_region(),
            composer,
        )),
        "wide" => Some(Region::split(
            Dir::Vertical,
            Constraint::Cells(1),
            Region::view(crate::modules::status::ID),
            Region::split(
                Dir::Vertical,
                Constraint::Fill,
                Region::split(
                    Dir::Horizontal,
                    Constraint::Percent(65),
                    crate::host::scroll_region(),
                    Region::view("findings"),
                ),
                composer,
            ),
        )),
        _ => None,
    }
}

/// The layout, and what it takes to change it.
///
/// Undo is the previous tree rather than an inverse op: an inverse that had to
/// reconstruct the shape would be a second implementation of every op, and the
/// two would drift.
pub struct Layout {
    tree: RwLock<Region>,
    history: RwLock<Vec<Region>>,
}

impl Layout {
    pub fn new(tree: Region) -> Self {
        Self {
            tree: RwLock::new(tree),
            history: RwLock::new(Vec::new()),
        }
    }

    pub fn tree(&self) -> Region {
        self.tree.read().expect("layout poisoned").clone()
    }

    pub fn set(&self, tree: Region) {
        *self.tree.write().expect("layout poisoned") = tree;
    }

    /// Apply one op. Never panics, never leaves an invalid tree, and never
    /// silently does nothing — a refusal always says why.
    pub fn apply(&self, op: &LayoutOp, known: &[String]) -> Result<String, LayoutError> {
        let current = self.tree();
        let on_screen: Vec<String> = current.modules();

        // A module riding the tail has no leaf of its own, so an op that aims
        // at one has nothing to move or resize. Refused up front rather than
        // left to fall through: `swap todo stream` used to replace the *stream*
        // node with `view("todo")` — deleting the conversation — and answer
        // `Ok("换位")`.
        for t in targets(op) {
            if let Target::Module(m) = t {
                if current.tail().contains(m) {
                    return Err(LayoutError::TailIsNotATarget(m.clone()));
                }
            }
        }

        let next = match op {
            LayoutOp::Undo => {
                let previous = self
                    .history
                    .write()
                    .expect("layout poisoned")
                    .pop()
                    .ok_or(LayoutError::NothingToUndo)?;
                *self.tree.write().expect("layout poisoned") = previous;
                return Ok("撤销了上一次布局改动".into());
            }
            LayoutOp::Show { module, side, size } => {
                if !known.iter().any(|k| k == module) {
                    return Err(LayoutError::NoSuchModule {
                        name: module.clone(),
                        available: known.to_vec(),
                    });
                }
                if on_screen.contains(module) {
                    return Err(LayoutError::AlreadyOnScreen(module.clone()));
                }
                let leaf = Region::view(module.clone());
                let at = Constraint::Cells(size.unwrap_or(1).max(1));
                if side.first() {
                    Region::split(side.dir(), at, leaf, current.clone())
                } else {
                    // `Cells` sizes the first child, so put the newcomer first
                    // and let the rest fill — the alternative is a constraint
                    // that means different things depending on the side.
                    Region::split(side.dir(), Constraint::Fill, current.clone(), leaf)
                }
            }
            LayoutOp::Hide { module } => {
                if !on_screen.contains(module) {
                    return Err(LayoutError::NotOnScreen(module.clone()));
                }
                current.prune(&|id| id != module)
            }
            LayoutOp::Swap { a, b } => {
                for t in [a, b] {
                    if let Target::Module(m) = t {
                        if !on_screen.contains(m) {
                            return Err(LayoutError::NotOnScreen(m.clone()));
                        }
                    }
                }
                swap(&current, a, b)
            }
            LayoutOp::Resize { target, size } => match resize(&current, target, *size) {
                (next, true) => next,
                (_, false) => return Err(LayoutError::NotOnScreen(target_label(target))),
            },
            LayoutOp::Preset { name } => preset(name).ok_or_else(|| LayoutError::NoSuchPreset {
                name: name.clone(),
                available: presets().iter().map(|(n, _)| n.to_string()).collect(),
            })?,
        };

        // Checked on the tree that would be installed, not on the op: the op is
        // only one of the ways a tree arrives (a preset is another, and the
        // default layout is a third), and the fault is a property of the tree.
        if let Some(id) = next.named_twice() {
            return Err(LayoutError::NamedTwice(id));
        }

        self.history.write().expect("layout poisoned").push(current);
        *self.tree.write().expect("layout poisoned") = next;
        Ok(describe(op))
    }

    /// What the model is told, so its picture and the screen cannot diverge.
    pub fn describe_for_model(&self, known: &[String]) -> String {
        let on = self.tree().modules();
        let off: Vec<&String> = known.iter().filter(|k| !on.contains(k)).collect();
        format!(
            "## 屏幕布局\n\n\
             当前显示:{}{}\n\
             可用但未显示:{}\n\n\
             用 `adjust_layout` 调整。例如把 findings 收起来就是 \
             `{{\"op\":\"hide\",\"module\":\"findings\"}}`,放到右边就是 \
             `{{\"op\":\"show\",\"module\":\"findings\",\"side\":\"right\",\"size\":30}}`。\n\
             命名布局:{}",
            on.join(" "),
            if self.tree().has_stream() {
                " 以及对话本身"
            } else {
                ""
            },
            if off.is_empty() {
                "(没有)".to_string()
            } else {
                off.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")
            },
            presets()
                .iter()
                .map(|(n, d)| format!("{n}({d})"))
                .collect::<Vec<_>>()
                .join(" · ")
        )
    }
}

fn describe(op: &LayoutOp) -> String {
    match op {
        LayoutOp::Show { module, side, .. } => format!("{module} → {side:?}"),
        LayoutOp::Hide { module } => format!("收起 {module}"),
        LayoutOp::Swap { .. } => "换位".into(),
        LayoutOp::Resize { size, .. } => format!("改成 {size}"),
        LayoutOp::Preset { name } => format!("布局 → {name}"),
        LayoutOp::Undo => "撤销".into(),
    }
}

/// The targets an op aims at, so the tail check has one place to look.
fn targets(op: &LayoutOp) -> Vec<&Target> {
    match op {
        LayoutOp::Swap { a, b } => vec![a, b],
        LayoutOp::Resize { target, .. } => vec![target],
        _ => Vec::new(),
    }
}

/// What to call a target in a message. `Target::Stream` is not a module, so it
/// has no name of its own to report.
fn target_label(target: &Target) -> String {
    match target {
        Target::Stream => "对话".to_string(),
        Target::Module(m) => m.clone(),
    }
}

fn matches(region: &Region, target: &Target) -> bool {
    match (region, target) {
        (Region::Stream { .. }, Target::Stream) => true,
        (Region::Module(id), Target::Module(m)) => id == m,
        _ => false,
    }
}

/// Swap what two targets are.
///
/// Each side is replaced by the node the **other** target actually matched,
/// cloned whole — not rebuilt from the `Target`. Rebuilding is what lost a
/// stream's tail: `Target::Stream` says "the conversation" and a rebuilt
/// `Region::stream()` is a conversation with nothing riding it, so swapping the
/// stream with anything quietly dropped every tail id.
fn swap(root: &Region, a: &Target, b: &Target) -> Region {
    swap_in(root, root, a, b)
}

fn swap_in(region: &Region, root: &Region, a: &Target, b: &Target) -> Region {
    match region {
        // Both sides come from `root`, not from the subtree being walked: the
        // two things being swapped are usually in different branches, and a
        // lookup that only saw the current branch would find neither and leave
        // the tree half-swapped.
        r if matches(r, a) => find(root, b).unwrap_or_else(|| r.clone()),
        r if matches(r, b) => find(root, a).unwrap_or_else(|| r.clone()),
        Region::Flex { dir, items, gap } => Region::Flex {
            dir: *dir,
            gap: *gap,
            items: items
                .iter()
                .map(|it| Item {
                    basis: it.basis,
                    grow: it.grow,
                    el: swap_in(&it.el, root, a, b),
                })
                .collect(),
        },
        Region::Stack(c) => Region::Stack(c.iter().map(|r| swap_in(r, root, a, b)).collect()),
        other => other.clone(),
    }
}

/// The node a target names, whole and as it stands.
///
/// The point is that it is *found* rather than reconstructed: a
/// `Region::stream()` built from `Target::Stream` would be a conversation with
/// no tail riding it, and a stream's tail is not expressible in the target.
fn find(region: &Region, target: &Target) -> Option<Region> {
    match region {
        r if matches(r, target) => Some(r.clone()),
        Region::Flex { items, .. } => items.iter().find_map(|it| find(&it.el, target)),
        Region::Stack(children) => children.iter().find_map(|c| find(c, target)),
        _ => None,
    }
}

/// Give `target` an explicit size wherever it sits.
///
/// The two-child version could only do this to the *first* child: hitting the
/// second set the constraint to `Fill`, which sizes the first and leaves the
/// second at whatever it asked for — so `/resize input 5` on a second child
/// silently did nothing. With children in a list, the one that was named is the
/// one that changes, whichever position it holds.
/// Give `target` an explicit size where it sits, and say whether anything
/// matched.
///
/// The `bool` is not decoration: a resize that matched nothing must not come
/// back as success. `apply` promises never to silently do nothing, and the tree
/// this returns for a miss is the tree it was handed — indistinguishable, to a
/// caller, from a resize that worked.
fn resize(region: &Region, target: &Target, size: u16) -> (Region, bool) {
    match region {
        Region::Flex { dir, items, gap } => {
            let hit = items.iter().position(|it| matches(&it.el, target));
            match hit {
                Some(i) => {
                    let mut items: Vec<Item> = items.clone();
                    items[i].basis = Constraint::Cells(size.max(1));
                    items[i].grow = 0;
                    // Something has to absorb what the resize gave up, or the
                    // box stops filling its area. If the sized child was the
                    // only elastic one, hand that job to a neighbour.
                    if items.iter().all(|it| it.grow == 0) {
                        if let Some(other) = (0..items.len()).find(|&k| k != i) {
                            items[other].grow = 1;
                        }
                    }
                    (
                        Region::Flex {
                            dir: *dir,
                            gap: *gap,
                            items,
                        },
                        true,
                    )
                }
                None => {
                    let mut found = false;
                    let items = items
                        .iter()
                        .map(|it| {
                            let (el, hit) = resize(&it.el, target, size);
                            found |= hit;
                            Item {
                                basis: it.basis,
                                grow: it.grow,
                                el,
                            }
                        })
                        .collect();
                    (
                        Region::Flex {
                            dir: *dir,
                            gap: *gap,
                            items,
                        },
                        found,
                    )
                }
            }
        }
        Region::Stack(c) => {
            let mut found = false;
            let children = c
                .iter()
                .map(|r| {
                    let (el, hit) = resize(r, target, size);
                    found |= hit;
                    el
                })
                .collect();
            (Region::Stack(children), found)
        }
        other => (other.clone(), false),
    }
}

#[cfg(test)]
mod tests {

    /// The bug the two-child form hid.
    ///
    /// `resize` used to change the constraint that sizes the *first* child, so
    /// naming the second one set `Fill` — which sizes the first and leaves the
    /// second at whatever it asked for. The command reported success and the
    /// screen did not move.
    #[test]
    fn resize_works_on_a_child_that_is_not_the_first() {
        let layout = Layout::new(crate::host::default_layout());
        let known = known();
        let before = layout
            .tree()
            .layout_with(crate::frame::Rect::sized(80, 24), &|_| 3);
        let input_h = |placed: &[(Region, crate::frame::Rect)]| {
            placed
                .iter()
                .find(|(e, _)| matches!(e, Region::Module(id) if id == crate::modules::input::ID))
                .map(|(_, r)| r.h)
                .expect("input is on screen")
        };
        assert_ne!(input_h(&before), 7);

        layout
            .apply(
                &LayoutOp::Resize {
                    target: Target::Module(crate::modules::input::ID.to_string()),
                    size: 7,
                },
                &known,
            )
            .expect("resize");
        let after = layout
            .tree()
            .layout_with(crate::frame::Rect::sized(80, 24), &|_| 3);
        assert_eq!(
            input_h(&after),
            7,
            "the named child is the one that changed"
        );
    }
    use super::*;

    fn known() -> Vec<String> {
        ["status", "input", "mascot", "findings"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn layout() -> Layout {
        Layout::new(crate::host::default_layout())
    }

    #[test]
    fn showing_and_hiding_are_exact_inverses() {
        let l = layout();
        let before = l.tree();
        l.apply(
            &LayoutOp::Show {
                module: "mascot".into(),
                side: Side::Top,
                size: Some(1),
            },
            &known(),
        )
        .unwrap();
        assert!(l.tree().modules().contains(&"mascot".to_string()));
        l.apply(
            &LayoutOp::Hide {
                module: "mascot".into(),
            },
            &known(),
        )
        .unwrap();
        assert_eq!(l.tree(), before, "back to exactly where it started");
    }

    #[test]
    fn undo_restores_the_previous_shape_exactly() {
        let l = layout();
        let before = l.tree();
        l.apply(
            &LayoutOp::Preset {
                name: "wide".into(),
            },
            &known(),
        )
        .unwrap();
        assert_ne!(l.tree(), before);
        l.apply(&LayoutOp::Undo, &known()).unwrap();
        assert_eq!(l.tree(), before);
    }

    #[test]
    fn undo_with_nothing_to_undo_says_so_rather_than_doing_something() {
        let l = layout();
        assert_eq!(
            l.apply(&LayoutOp::Undo, &known()),
            Err(LayoutError::NothingToUndo)
        );
    }

    #[test]
    fn a_wrong_module_name_lists_the_right_ones() {
        let l = layout();
        match l.apply(
            &LayoutOp::Show {
                module: "nope".into(),
                side: Side::Right,
                size: None,
            },
            &known(),
        ) {
            Err(e @ LayoutError::NoSuchModule { .. }) => {
                let m = e.to_string();
                assert!(m.contains("mascot") && m.contains("findings"), "{m}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn showing_something_already_shown_is_refused_not_duplicated() {
        let l = layout();
        assert_eq!(
            l.apply(
                &LayoutOp::Show {
                    module: "status".into(),
                    side: Side::Top,
                    size: None
                },
                &known()
            ),
            Err(LayoutError::AlreadyOnScreen("status".into()))
        );
        assert_eq!(
            l.tree().modules().iter().filter(|m| *m == "status").count(),
            1
        );
    }

    #[test]
    fn swapping_twice_is_the_identity() {
        let l = layout();
        let before = l.tree();
        let op = LayoutOp::Swap {
            a: Target::Module("status".into()),
            b: Target::Module("input".into()),
        };
        l.apply(&op, &known()).unwrap();
        assert_ne!(l.tree(), before);
        l.apply(&op, &known()).unwrap();
        assert_eq!(l.tree(), before, "swap is its own inverse");
    }

    /// The shape ADR 0020's Step 3 produces, built by hand.
    ///
    /// A tail is only a tree that declares one, and this build ships none yet —
    /// so there is nothing shipped to borrow here. It has to be the *shape* of
    /// the real thing rather than `default_layout()` with a tail bolted on: in
    /// that layout `todo` and `live` are still leaves of the composer, and a
    /// tree naming them both ways is the one thing `named_twice` refuses.
    fn layout_with_tail() -> Layout {
        let frame = Region::flex(
            Dir::Vertical,
            vec![
                Item::hug(Region::view("tip")),
                Item::grow(Region::view(crate::modules::input::ID)),
            ],
        );
        let below = Region::split(
            Dir::Vertical,
            Constraint::Fill,
            frame,
            Region::view(crate::modules::status::ID),
        );
        Layout::new(Region::split(
            Dir::Vertical,
            Constraint::Fill,
            Region::stream().with_tail(["todo", "live"]),
            below,
        ))
    }

    #[test]
    fn a_tail_survives_a_swap() {
        // `swap` used to rebuild the node it matched from the `Target` alone,
        // so `Target::Stream` produced a bare `Region::stream()` and the tail
        // went with it — todo and live simply vanished from the tree, on a
        // command the model can reach through `adjust_layout`.
        let l = layout_with_tail();
        let before = l.tree().tail().to_vec();
        assert_eq!(before, ["todo", "live"], "the fixture has a tail to lose");

        // `status` and `input` are the two leaves the shipped layout really
        // has, so this swaps the conversation with one of them — the case that
        // used to rebuild the stream and drop its tail.
        let before_tree = l.tree();
        l.apply(
            &LayoutOp::Swap {
                a: Target::Stream,
                b: Target::Module("status".into()),
            },
            &known(),
        )
        .unwrap();

        let mut after = l.tree().tail().to_vec();
        after.sort();
        assert_eq!(after, ["live", "todo"], "the tail went with the node");
        assert!(
            l.tree().has_stream(),
            "and swapping is a move, not a way to delete the conversation"
        );

        // Its own inverse, which is the other thing rebuilding broke: it put
        // back a bare stream, so the second swap could not restore the first.
        l.apply(
            &LayoutOp::Swap {
                a: Target::Stream,
                b: Target::Module("status".into()),
            },
            &known(),
        )
        .unwrap();
        assert_eq!(l.tree(), before_tree, "swap is still its own inverse");
    }

    #[test]
    fn a_tail_id_is_not_a_valid_target() {
        // A module riding the tail has no leaf of its own, so an op that aims
        // at one has nothing to aim at. `swap todo stream` used to replace the
        // *stream* node with `view("todo")`, which deletes the conversation and
        // still answers `Ok("换位")`.
        let l = layout_with_tail();
        let before = l.tree();

        let err = l
            .apply(
                &LayoutOp::Swap {
                    a: Target::Module("todo".into()),
                    b: Target::Stream,
                },
                &known(),
            )
            .expect_err("aiming at a tail id has to be refused, not guessed at");
        assert!(
            matches!(&err, LayoutError::TailIsNotATarget(m) if m == "todo"),
            "{err:?}"
        );
        assert_eq!(l.tree(), before, "and the tree is left alone");

        let err = l
            .apply(
                &LayoutOp::Resize {
                    target: Target::Module("live".into()),
                    size: 5,
                },
                &known(),
            )
            .expect_err("a tail id has no box to resize");
        assert!(
            matches!(&err, LayoutError::TailIsNotATarget(m) if m == "live"),
            "{err:?}"
        );
        assert_eq!(l.tree(), before);
    }

    #[test]
    fn an_op_that_changes_nothing_is_not_reported_as_success() {
        // `apply` promises never to silently do nothing. `resize` on a target
        // that is not there walks the whole tree, finds no flex child, and
        // returns the tree it was given along with `Ok("改成 5")` — the caller
        // has no way to tell that from a resize that worked.
        let l = layout();
        let err = l
            .apply(
                &LayoutOp::Resize {
                    target: Target::Module("findings".into()),
                    size: 5,
                },
                &known(),
            )
            .expect_err("nothing was resized, so nothing succeeded");
        assert!(matches!(err, LayoutError::NotOnScreen(_)), "{err:?}");
    }

    #[test]
    fn every_op_leaves_a_tree_that_still_lays_out() {
        use crate::frame::Rect;
        let l = layout();
        let ops = [
            LayoutOp::Show {
                module: "mascot".into(),
                side: Side::Left,
                size: Some(20),
            },
            LayoutOp::Show {
                module: "findings".into(),
                side: Side::Bottom,
                size: Some(5),
            },
            LayoutOp::Resize {
                target: Target::Module("findings".into()),
                size: 3,
            },
            LayoutOp::Swap {
                a: Target::Stream,
                b: Target::Module("findings".into()),
            },
            LayoutOp::Hide {
                module: "status".into(),
            },
            LayoutOp::Preset {
                name: "focus".into(),
            },
            LayoutOp::Undo,
        ];
        for op in &ops {
            let _ = l.apply(op, &known());
            for w in [0u16, 1, 3, 40, 200] {
                for h in [0u16, 1, 3, 24, 60] {
                    for (_, r) in l.tree().layout(Rect::sized(w, h)) {
                        assert!(r.right() <= w && r.bottom() <= h, "{op:?} at {w}×{h}");
                    }
                }
            }
        }
    }

    #[test]
    fn what_the_model_is_told_comes_from_the_same_tree_the_screen_uses() {
        let l = layout();
        let told = l.describe_for_model(&known());
        assert!(told.contains("status") && told.contains("input"));
        assert!(told.contains("mascot"), "unshown ones are listed too");
        l.apply(
            &LayoutOp::Hide {
                module: "status".into(),
            },
            &known(),
        )
        .unwrap();
        let after = l.describe_for_model(&known());
        assert!(
            after.contains("可用但未显示:")
                && after
                    .split("可用但未显示:")
                    .nth(1)
                    .unwrap()
                    .contains("status"),
            "the description follows the tree:\\n{after}"
        );
    }

    #[test]
    fn an_op_round_trips_through_json_the_way_a_model_would_send_it() {
        let op: LayoutOp =
            serde_json::from_str(r#"{"op":"show","module":"findings","side":"right","size":30}"#)
                .expect("a model's json parses");
        assert_eq!(
            op,
            LayoutOp::Show {
                module: "findings".into(),
                side: Side::Right,
                size: Some(30)
            }
        );
        let back = serde_json::to_string(&op).unwrap();
        assert_eq!(serde_json::from_str::<LayoutOp>(&back).unwrap(), op);
    }

    #[test]
    fn a_malformed_op_is_a_parse_error_not_a_panic() {
        for bad in [
            r#"{"op":"show"}"#,
            r#"{"op":"nonsense"}"#,
            "{}",
            "null",
            "[]",
        ] {
            assert!(serde_json::from_str::<LayoutOp>(bad).is_err(), "{bad}");
        }
    }
}

#[cfg(test)]
mod three_ways {
    //! The gate that stops three entry points becoming three implementations.
    //!
    //! A key, a slash command and the model's tool all mean the same thing by
    //! "hide the status bar". If they ever stop meaning the same thing, this is
    //! where it shows.

    use super::*;
    use crate::keymap::{Action, Default_, Keymap};
    use crate::surface::KeyPress;

    fn known() -> Vec<String> {
        ["status", "input", "mascot", "findings"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// The op a key produces.
    fn by_key(press: KeyPress) -> Option<LayoutOp> {
        Default_
            .bindings()
            .into_iter()
            .find(|(k, _)| *k == press)
            .and_then(|(_, a)| match a {
                Action::Layout(op) => Some(op),
                _ => None,
            })
    }

    /// The op the model's JSON produces.
    fn by_model(json: &str) -> LayoutOp {
        serde_json::from_str(json).expect("the model's json parses")
    }

    #[test]
    fn a_key_and_the_model_agree_on_what_focus_means() {
        assert_eq!(
            by_key(KeyPress::ctrl('f')).expect("ctrl-f is bound"),
            by_model(r#"{"op":"preset","name":"focus"}"#)
        );
    }

    #[test]
    fn a_key_and_the_model_agree_on_what_undo_means() {
        assert_eq!(
            by_key(KeyPress::ctrl('z')).expect("ctrl-z is bound"),
            by_model(r#"{"op":"undo"}"#)
        );
    }

    #[test]
    fn the_same_op_from_any_source_leaves_the_same_screen() {
        // Three layouts, three routes, one shape at the end.
        let op = LayoutOp::Hide {
            module: "status".into(),
        };
        let from_key = Layout::new(crate::host::default_layout());
        from_key.apply(&op, &known()).unwrap();

        let from_command = Layout::new(crate::host::default_layout());
        from_command
            .apply(
                &LayoutOp::Hide {
                    module: "status".into(),
                },
                &known(),
            )
            .unwrap();

        let from_model = Layout::new(crate::host::default_layout());
        from_model
            .apply(&by_model(r#"{"op":"hide","module":"status"}"#), &known())
            .unwrap();

        assert_eq!(from_key.tree(), from_command.tree());
        assert_eq!(from_key.tree(), from_model.tree());
    }

    #[test]
    fn a_layout_change_never_touches_what_a_module_has_folded() {
        use crate::module::{Modules, Mounted};
        use std::sync::Arc;

        // The property the two axes are orthogonal on: move a module and its
        // state comes with it, unchanged.
        let mods = Arc::new(Modules::new());
        mods.add_view(Arc::new(Mounted::<crate::modules::status::Status>::new()))
            .unwrap();
        let view = mods.view("status").unwrap();
        for f in crate::conformance::facts() {
            view.absorb(&f);
        }
        let moment = crate::moment::Moment::default();
        let vp = crate::moment::Viewport::new(crate::frame::Rect::sized(60, 1), &moment);
        let before = view.render(&vp);

        let l = Layout::new(crate::host::default_layout());
        for op in [
            LayoutOp::Hide {
                module: "status".into(),
            },
            LayoutOp::Show {
                module: "status".into(),
                side: Side::Bottom,
                size: Some(1),
            },
            LayoutOp::Preset {
                name: "wide".into(),
            },
            LayoutOp::Undo,
        ] {
            let _ = l.apply(&op, &known());
        }

        assert_eq!(view.render(&vp), before, "space moved; the fold did not");
    }
}
