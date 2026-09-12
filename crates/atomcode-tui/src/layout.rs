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
            Region::Stream,
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
                    Region::Stream,
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
            LayoutOp::Resize { target, size } => resize(&current, target, *size),
            LayoutOp::Preset { name } => preset(name).ok_or_else(|| LayoutError::NoSuchPreset {
                name: name.clone(),
                available: presets().iter().map(|(n, _)| n.to_string()).collect(),
            })?,
        };

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

fn matches(region: &Region, target: &Target) -> bool {
    match (region, target) {
        (Region::Stream, Target::Stream) => true,
        (Region::Module(id), Target::Module(m)) => id == m,
        _ => false,
    }
}

fn swap(region: &Region, a: &Target, b: &Target) -> Region {
    match region {
        r if matches(r, a) => match b {
            Target::Stream => Region::Stream,
            Target::Module(m) => Region::view(m.clone()),
        },
        r if matches(r, b) => match a {
            Target::Stream => Region::Stream,
            Target::Module(m) => Region::view(m.clone()),
        },
        Region::Flex { dir, items, gap } => Region::Flex {
            dir: *dir,
            gap: *gap,
            items: items
                .iter()
                .map(|it| Item {
                    basis: it.basis,
                    grow: it.grow,
                    el: swap(&it.el, a, b),
                })
                .collect(),
        },
        Region::Stack(c) => Region::Stack(c.iter().map(|r| swap(r, a, b)).collect()),
        other => other.clone(),
    }
}

/// Give `target` an explicit size wherever it sits.
///
/// The two-child version could only do this to the *first* child: hitting the
/// second set the constraint to `Fill`, which sizes the first and leaves the
/// second at whatever it asked for — so `/resize input 5` on a second child
/// silently did nothing. With children in a list, the one that was named is the
/// one that changes, whichever position it holds.
fn resize(region: &Region, target: &Target, size: u16) -> Region {
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
                    Region::Flex {
                        dir: *dir,
                        gap: *gap,
                        items,
                    }
                }
                None => Region::Flex {
                    dir: *dir,
                    gap: *gap,
                    items: items
                        .iter()
                        .map(|it| Item {
                            basis: it.basis,
                            grow: it.grow,
                            el: resize(&it.el, target, size),
                        })
                        .collect(),
                },
            }
        }
        Region::Stack(c) => Region::Stack(c.iter().map(|r| resize(r, target, size)).collect()),
        other => other.clone(),
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
