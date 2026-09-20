//! Where a panel sits on screen.
//!
//! A panel row puts itself on screen when it mounts and takes itself off when
//! it unmounts, through [`LayoutOp::Show`] and [`LayoutOp::Hide`]. That is
//! rendering assembly, not a person or a model rearranging the screen: the
//! adjustable layout — presets, swaps, resizes, undo, the `adjust_layout` tool
//! and the commands and keys that reached them — was taken out until it is
//! thought through (`docs/adr/0022` §8; `docs/adr/0007` is void).

use crate::i18n::{t, Msg};
use std::sync::RwLock;

use crate::region::{Constraint, Dir, Region};

/// Where a module goes when it is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// A panel putting itself on screen, or taking itself off.
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

/// Why an op could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutError {
    NoSuchModule {
        name: String,
        available: Vec<String>,
    },
    NotOnScreen(String),
    AlreadyOnScreen(String),
    /// One module named both as a tail id and as a leaf of its own, which would
    /// draw it twice. See [`El::named_twice`](crate::el::El::named_twice).
    NamedTwice(String),
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::NoSuchModule { name, available } => {
                let available = available.join(" ");
                write!(
                    f,
                    "{}",
                    t(Msg::LayoutNoSuchModule {
                        name,
                        available: &available
                    })
                )
            }
            LayoutError::NotOnScreen(m) => {
                write!(f, "{}", t(Msg::LayoutNotOnScreen { module: m }))
            }
            LayoutError::AlreadyOnScreen(m) => {
                write!(f, "{}", t(Msg::LayoutAlreadyOnScreen { module: m }))
            }
            LayoutError::NamedTwice(m) => {
                write!(f, "{}", t(Msg::LayoutDrawnTwice { module: m }))
            }
        }
    }
}

/// The region tree on screen.
pub struct Layout {
    tree: RwLock<Region>,
}

impl Layout {
    pub fn new(tree: Region) -> Self {
        Self {
            tree: RwLock::new(tree),
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
    pub fn apply(&self, op: &LayoutOp, known: &[String]) -> Result<(), LayoutError> {
        let current = self.tree();
        let on_screen: Vec<String> = current.modules();

        let next = match op {
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
        };

        // Checked on the tree that would be installed, not on the op: the
        // default layout is another way a tree arrives, and the fault is a
        // property of the tree.
        if let Some(id) = next.named_twice() {
            return Err(LayoutError::NamedTwice(id));
        }

        *self.tree.write().expect("layout poisoned") = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
            LayoutOp::Hide {
                module: "status".into(),
            },
            LayoutOp::Hide {
                module: "mascot".into(),
            },
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
        ] {
            let _ = l.apply(&op, &known());
        }

        assert_eq!(view.render(&vp), before, "space moved; the fold did not");
    }
}
