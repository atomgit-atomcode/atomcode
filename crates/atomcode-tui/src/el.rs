//! Layout primitives: the small declarative core, borrowed from React.
//!
//! A module used to return `Vec<Line>` and do its own width arithmetic. The
//! status bar's "left flush, right flush" was six lines of `saturating_sub` and
//! `take_width`, and every module that wanted the same thing wrote it again.
//! That is the cost of having no primitives — not that the code is long, but
//! that alignment, truncation and padding are re-derived per module and each
//! derivation is a chance to get it wrong.
//!
//! So: build a tree, hand it a width, get lines.
//!
//! ```ignore
//! El::row(vec![
//!     El::text(Line::raw("atomcode · working")),
//!     El::Spacer,
//!     El::text(Line::raw("turn 3 · 12k")),
//! ])
//! .lay(80)
//! ```
//!
//! # What is deliberately NOT borrowed from React
//!
//! **No hooks, no component-local mutable state.** Module state is a fold over
//! the session log (`View::absorb`), which is what makes a replay reproduce the
//! screen cell for cell. A component holding private state would break that
//! silently — the tests would stay green and stop meaning anything. Components
//! here are plain functions of their arguments.
//!
//! **No virtual DOM, no reconciliation.** A terminal frame is a couple of
//! thousand cells and the whole thing is recomposed per paint. Diffing would buy
//! nothing and cost a cache, and a cache is a thing that can be stale — the
//! opposite of what `block::ContentHash` exists to guarantee.
//!
//! # Shape
//!
//! `Row` puts its children side by side **on one line**; `Col` stacks blocks of
//! lines. That asymmetry is on purpose: a horizontal band of multi-line children
//! needs column arbitration, which is what the *region tree* already does one
//! level up. Two layout engines for one screen is one too many.

use crate::frame::{Color, Line, Rect, Span, Style};
use crate::theme::Role;
use crate::width;

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

/// One child of a [`El::Flex`], with its share of the main axis.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Starting size along the main axis, before any leftover is handed out.
    /// `Fill` means "whatever the content asks for".
    pub basis: Constraint,
    /// Share of the leftover, as a weight. `0` never grows.
    pub grow: u16,
    pub el: El,
}

impl Item {
    /// Exactly this size, and it does not grow.
    pub fn fixed(cells: u16, el: El) -> Item {
        Item {
            basis: Constraint::Cells(cells),
            grow: 0,
            el,
        }
    }
    /// Content-sized, and it takes the leftover.
    pub fn grow(el: El) -> Item {
        Item {
            basis: Constraint::Fill,
            grow: 1,
            el,
        }
    }
    /// Content-sized, and it does not grow.
    pub fn hug(el: El) -> Item {
        Item {
            basis: Constraint::Fill,
            grow: 0,
            el,
        }
    }
}

/// A piece of screen.
///
/// Two kinds of node, and the distinction is the same one HTML makes:
///
/// * **块级** — [`El::Flex`], [`El::Stack`], [`El::Module`], [`El::Stream`].
///   These divide a *rect*. [`El::place`] walks them and hands each leaf an
///   area. This is what used to be a separate `El` type with its own
///   engine; there is now one node type and one engine.
/// * **行内** — [`El::Text`], [`El::Row`], [`El::Col`], [`El::Spacer`],
///   [`El::Fixed`], [`El::Indent`], [`El::Framed`]. These divide a *width*.
///   [`El::lay`] turns them into lines.
///
/// A module returns an inline tree; the screen is a block tree whose leaves are
/// modules. Because they are the same type, a module can return a tree that
/// itself names modules — nesting, which two separate types could not express.
#[derive(Clone, Debug, PartialEq)]
pub enum El {
    /// Nothing. Renders to no lines, takes no space in a `Row`; what a split
    /// collapses to when a module is not mounted.
    Empty,
    /// 块级：a view module, by id. The host resolves the name against the
    /// mounted rows *for a realm*, so the tree says where and the realm says
    /// which.
    Module(String),
    /// 块级：the irreversible stream.
    Stream,
    /// 块级：any number of children dividing an area — flexbox.
    ///
    /// The web's model, minus the parts a terminal should not have. `basis`
    /// is `flex-basis`, `grow` is `flex-grow`, `gap` is `gap`, and `dir` is
    /// `flex-direction`. `justify-content` needs no field: [`El::Spacer`]
    /// already expresses space-between and centring, and it composes.
    ///
    /// Deliberately absent: `flex-wrap` (a terminal row that wraps is a layout
    /// nobody can read, and it would end the "every row is exactly `w` cells"
    /// invariant that catches real bugs) and `order` (the tree order is what a
    /// person edits and what `describe_for_model` reports; a second ordering
    /// would make those two disagree).
    ///
    /// `align-items` is absent too, but only for now and for a concrete
    /// reason: cross-axis alignment needs each child's *cross-axis* wanted
    /// size, and modules only express a height. Adding the knob before
    /// anything can answer that question would be a knob that silently does
    /// nothing.
    Flex {
        dir: Dir,
        items: Vec<Item>,
        /// Cells between adjacent children.
        gap: u16,
    },
    /// 块级：overlaid, later on top. Exactly one may hold focus.
    Stack(Vec<El>),
    /// One line of styled text.
    Text(Line),
    /// Children side by side, left to right, on a single line.
    Row(Vec<El>),
    /// Children stacked, top to bottom.
    Col(Vec<El>),
    /// Eats whatever width is left in a `Row`. Several share it evenly — which
    /// is how you centre something: `Row[Spacer, thing, Spacer]`.
    Spacer,
    /// Exactly this many cells wide inside a `Row`; padded or truncated to fit.
    Fixed(u16, Box<El>),
    /// A box, with an optional caption on the top edge and another on the
    /// bottom. The footer is the useful half: a hint on the bottom rule reads
    /// as chrome, while the same hint on its own line reads as content.
    Framed {
        title: Option<String>,
        footer: Option<String>,
        child: Box<El>,
    },
    /// Indent every line of the child.
    Indent(u16, Box<El>),
    /// Give every span underneath a style it does not already state — a
    /// background for a whole bar, a dim for a whole block. Inheritance, so a
    /// child never has to know what it is sitting on.
    Styled(Style, Box<El>),
}

impl El {
    pub fn text(line: Line) -> El {
        El::Text(line)
    }
    pub fn raw(s: impl Into<String>) -> El {
        El::Text(Line::raw(s))
    }
    pub fn styled(s: impl Into<String>, style: Style) -> El {
        El::Text(Line::styled(s, style))
    }
    /// 块级：name a module. The registry resolves it at compose time.
    pub fn view(id: impl Into<String>) -> El {
        El::Module(id.into())
    }

    /// 块级：divide an area between two children — the two-child special case
    /// of [`El::Flex`], kept because most layouts are exactly this and because
    /// every existing call site and layout op speaks it.
    ///
    /// `at` sizes the *first* child; whichever child is not sized takes the
    /// leftover. That is the same meaning it had when this was `Region::Split`.
    pub fn split(dir: Dir, at: Constraint, a: El, b: El) -> El {
        let (ga, gb) = if matches!(at, Constraint::Fill) {
            (1, 0)
        } else {
            (0, 1)
        };
        El::Flex {
            dir,
            gap: 0,
            items: vec![
                Item {
                    basis: at,
                    grow: ga,
                    el: a,
                },
                Item {
                    basis: Constraint::Fill,
                    grow: gb,
                    el: b,
                },
            ],
        }
    }

    /// 块级：any number of children.
    pub fn flex(dir: Dir, items: Vec<Item>) -> El {
        El::Flex { dir, items, gap: 0 }
    }

    /// Space between children, in cells.
    pub fn gap(self, cells: u16) -> El {
        match self {
            El::Flex { dir, items, .. } => El::Flex {
                dir,
                items,
                gap: cells,
            },
            other => other,
        }
    }

    pub fn row(children: Vec<El>) -> El {
        El::Row(children)
    }
    pub fn col(children: Vec<El>) -> El {
        El::Col(children)
    }
    /// Everything underneath inherits this style where it states none.
    pub fn styled_all(style: Style, child: El) -> El {
        El::Styled(style, Box::new(child))
    }

    pub fn framed(child: El) -> El {
        El::Framed {
            title: None,
            footer: None,
            child: Box::new(child),
        }
    }
    pub fn title(self, t: impl Into<String>) -> El {
        match self {
            El::Framed { footer, child, .. } => El::Framed {
                title: Some(t.into()),
                footer,
                child,
            },
            other => El::Framed {
                title: Some(t.into()),
                footer: None,
                child: Box::new(other),
            },
        }
    }
    pub fn footer(self, f: impl Into<String>) -> El {
        match self {
            El::Framed { title, child, .. } => El::Framed {
                title,
                footer: Some(f.into()),
                child,
            },
            other => El::Framed {
                title: None,
                footer: Some(f.into()),
                child: Box::new(other),
            },
        }
    }

    /// Stream on top, `below` underneath, `below` taking `rows`.
    pub fn stream_over(below: El, rows: u16) -> El {
        El::flex(
            Dir::Vertical,
            vec![Item::grow(El::Stream), Item::fixed(rows, below)],
        )
    }

    /// Every module id this tree names, in tree order.
    pub fn modules(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(&mut |r| {
            if let El::Module(id) = r {
                out.push(id.clone());
            }
        });
        out
    }

    pub fn has_stream(&self) -> bool {
        let mut found = false;
        self.walk(&mut |r| {
            if matches!(r, El::Stream) {
                found = true;
            }
        });
        found
    }

    fn walk(&self, f: &mut impl FnMut(&El)) {
        f(self);
        match self {
            El::Flex { items, .. } => items.iter().for_each(|it| it.el.walk(f)),
            El::Stack(children) => children.iter().for_each(|c| c.walk(f)),
            _ => {}
        }
    }

    /// Drop leaves naming modules that are not mounted, collapsing the splits
    /// they leave behind.
    ///
    /// A layout that mentions `findings` must still work where that row is not
    /// mounted — a variant's layout has to survive being used by another
    /// variant. Collapsing, not panicking, is what makes that true.
    pub fn prune(&self, mounted: &dyn Fn(&str) -> bool) -> El {
        match self {
            El::Module(id) if !mounted(id) => El::Empty,
            El::Flex { dir, items, gap } => {
                let kept: Vec<Item> = items
                    .iter()
                    .map(|it| Item {
                        basis: it.basis,
                        grow: it.grow,
                        el: it.el.prune(mounted),
                    })
                    .filter(|it| !matches!(it.el, El::Empty))
                    .collect();
                match kept.len() {
                    0 => El::Empty,
                    // One survivor takes the whole area: a flex box around a
                    // single child is the child.
                    1 => kept.into_iter().next().expect("checked").el,
                    _ => El::Flex {
                        dir: *dir,
                        items: kept,
                        gap: *gap,
                    },
                }
            }
            El::Stack(children) => {
                let kept: Vec<_> = children
                    .iter()
                    .map(|c| c.prune(mounted))
                    .filter(|c| !matches!(c, El::Empty))
                    .collect();
                match kept.len() {
                    0 => El::Empty,
                    1 => kept.into_iter().next().unwrap(),
                    _ => El::Stack(kept),
                }
            }
            other => other.clone(),
        }
    }

    /// Assign a rect to every leaf, giving every module one row.
    pub fn layout(&self, area: Rect) -> Vec<(El, Rect)> {
        self.layout_with(area, &|_| 1)
    }

    /// Assign rects, asking `wants` how many rows each module would like.
    ///
    /// Arbitration lives here rather than in the modules: a module *requests* a
    /// height and the tree decides, so one module can never seize the screen —
    /// and `Fill` can leave exactly the right amount for what sits beside it,
    /// which pure geometry alone cannot know.
    pub fn layout_with(&self, area: Rect, wants: &dyn Fn(&str) -> u16) -> Vec<(El, Rect)> {
        let mut out = Vec::new();
        self.place_into(area, wants, &mut out);
        out
    }

    fn place_into(&self, area: Rect, wants: &dyn Fn(&str) -> u16, out: &mut Vec<(El, Rect)>) {
        if area.is_empty() {
            return;
        }
        match self {
            El::Empty => {}
            // Leaves of the block pass. An inline subtree sitting directly in
            // a split is a leaf too: it gets an area, and the host lays it at
            // that area's width.
            El::Stream
            | El::Module(_)
            | El::Text(_)
            | El::Row(_)
            | El::Col(_)
            | El::Spacer
            | El::Fixed(..)
            | El::Indent(..)
            | El::Styled(..)
            | El::Framed { .. } => out.push((self.clone(), area)),
            El::Stack(children) => children.iter().for_each(|c| c.place_into(area, wants, out)),
            El::Flex { dir, items, gap } => {
                if items.is_empty() {
                    return;
                }
                let total = match dir {
                    Dir::Vertical => area.h,
                    Dir::Horizontal => area.w,
                };
                let gaps = gap
                    .saturating_mul(items.len().saturating_sub(1) as u16)
                    .min(total);
                let avail = total.saturating_sub(gaps);

                // 1. flex-basis. `Fill` means "ask the content", which is how a
                //    child that is not explicitly sized still reserves what it
                //    needs before anyone grows into the rest.
                let mut sizes: Vec<u16> = items
                    .iter()
                    .map(|it| match it.basis {
                        Constraint::Cells(n) => n.min(avail),
                        Constraint::Percent(p) => ((avail as u32 * p.min(100) as u32) / 100) as u16,
                        Constraint::Fill => it.el.wanted(*dir, wants).min(avail),
                    })
                    .collect();

                let used: u32 = sizes.iter().map(|&s| s as u32).sum();
                let weight: u32 = items.iter().map(|it| it.grow as u32).sum();

                if used < avail as u32 && weight > 0 {
                    // 2. flex-grow. The last growing child takes the rounding,
                    //    so the children add up to exactly the area and the far
                    //    edge lands where it should.
                    let slack = avail as u32 - used;
                    let last = items.iter().rposition(|it| it.grow > 0);
                    let mut spent = 0u32;
                    for (i, it) in items.iter().enumerate() {
                        if it.grow == 0 {
                            continue;
                        }
                        let share = if Some(i) == last {
                            slack - spent
                        } else {
                            slack * it.grow as u32 / weight
                        };
                        spent += share;
                        sizes[i] = sizes[i].saturating_add(share as u16);
                    }
                } else if used > avail as u32 {
                    // 3. Overflow. Growing children give room back first —
                    //    they asked to be elastic — and only then the fixed
                    //    ones. Nobody gets a negative size and nothing panics:
                    //    a layout is data a user can write.
                    let mut over = used - avail as u32;
                    // Elastic children give room back first, in order — they
                    // asked to be elastic. Then the fixed ones, from the *end*
                    // backwards: what is declared first is usually what frames
                    // the screen (a status bar, a title), and a rule has to
                    // pick someone. Nobody goes negative and nothing panics —
                    // a layout is data a user can write.
                    let order: Vec<usize> = (0..items.len())
                        .filter(|&i| items[i].grow > 0)
                        .chain((0..items.len()).rev().filter(|&i| items[i].grow == 0))
                        .collect();
                    for i in order {
                        if over == 0 {
                            break;
                        }
                        let take = (sizes[i] as u32).min(over);
                        sizes[i] -= take as u16;
                        over -= take;
                    }
                }

                let mut off = 0u16;
                for (i, it) in items.iter().enumerate() {
                    let s = sizes[i];
                    let sub = match dir {
                        Dir::Vertical => Rect::new(area.x, area.y + off, area.w, s),
                        Dir::Horizontal => Rect::new(area.x + off, area.y, s, area.h),
                    };
                    it.el.place_into(sub, wants, out);
                    off = off
                        .saturating_add(s)
                        .saturating_add(if i + 1 < items.len() { *gap } else { 0 });
                }
            }
        }
    }

    /// How much a subtree asks for when the other side takes `Fill`.
    fn wanted(&self, dir: Dir, wants: &dyn Fn(&str) -> u16) -> u16 {
        match self {
            El::Empty => 0,
            El::Stream => 1,
            El::Module(id) => wants(id).max(1),
            // An inline subtree asks for one row. It could be laid to count
            // its lines, but not here: `wanted` has no width, and guessing one
            // would make the arbitration depend on a number nobody chose. A
            // caller that wants more gives the split an explicit constraint.
            El::Text(_)
            | El::Row(_)
            | El::Col(_)
            | El::Spacer
            | El::Fixed(..)
            | El::Indent(..)
            | El::Styled(..)
            | El::Framed { .. } => 1,
            El::Stack(c) => c.iter().map(|r| r.wanted(dir, wants)).max().unwrap_or(0),
            El::Flex { dir: d, items, gap } => {
                if *d == dir {
                    // Along the axis: everyone's basis, plus the gaps.
                    let content = items.iter().fold(0u16, |acc, it| {
                        let want = match it.basis {
                            Constraint::Cells(n) => n,
                            _ => it.el.wanted(dir, wants),
                        };
                        acc.saturating_add(want)
                    });
                    content.saturating_add(gap.saturating_mul(items.len().saturating_sub(1) as u16))
                } else {
                    // Across it: the widest child.
                    items
                        .iter()
                        .map(|it| it.el.wanted(dir, wants))
                        .max()
                        .unwrap_or(0)
                }
            }
        }
    }

    /// Render at this width. Every line comes back at most `w` cells wide —
    /// the containment invariant is upheld here rather than by each caller
    /// remembering to truncate.
    pub fn lay(&self, w: u16) -> Vec<Line> {
        if w == 0 {
            return Vec::new();
        }
        match self {
            El::Empty => Vec::new(),
            // A block-level node in an inline context. Unreachable in correct
            // use — `place` takes them and hands their leaves to the host —
            // and empty rather than a panic, because a layout is data a user
            // can write and bad data must not take the screen down.
            El::Module(_) | El::Stream | El::Flex { .. } | El::Stack(_) => Vec::new(),
            El::Text(line) => vec![line.truncate(w as usize)],
            El::Col(children) => children.iter().flat_map(|c| c.lay(w)).collect(),
            El::Row(children) => lay_row(children, w),
            El::Spacer => vec![Line::from_spans(vec![Span::raw(" ".repeat(w as usize))])],
            El::Fixed(cells, inner) => {
                let cells = (*cells).min(w);
                inner
                    .lay(cells)
                    .into_iter()
                    .map(|l| pad_to(l, cells as usize))
                    .collect()
            }
            El::Indent(by, child) => {
                let inner = w.saturating_sub(*by);
                if inner == 0 {
                    return Vec::new();
                }
                child
                    .lay(inner)
                    .into_iter()
                    .map(|l| {
                        let mut spans = vec![Span::raw(" ".repeat(*by as usize))];
                        spans.extend(l.spans.clone());
                        Line::from_spans(spans)
                    })
                    .collect()
            }
            El::Styled(style, child) => child
                .lay(w)
                .into_iter()
                .map(|l| {
                    Line::from_spans(
                        l.spans
                            .into_iter()
                            .map(|sp| Span::styled(sp.text, sp.style.under(*style)))
                            .collect(),
                    )
                })
                .collect(),
            El::Framed {
                title,
                footer,
                child,
            } => frame(title.as_deref(), footer.as_deref(), child, w),
        }
    }
}

/// Children side by side, each as tall as it needs.
///
/// A row used to be one line, and the doc said a band of multi-line children
/// was the region tree's job. That was true when there were two trees; it is
/// not what a widget needs. A list beside a preview, or content beside a
/// scrollbar, is one module's inside — the tree above it sees one module.
///
/// So a row lays each child at its own width and then zips the results: row `i`
/// is every child's line `i`, padded to its column. Single-line children behave
/// exactly as before, which is why this is a generalisation and not a change.
///
/// **Each child is laid exactly once.** The first version measured a child by
/// laying it and then laid it again to draw — doubling the work at every level,
/// so a tree 200 deep cost 2^200 and the test suite stopped returning. It did
/// not fail; it hung, which is the failure mode that gets mistaken for slowness.
fn lay_row(children: &[El], w: u16) -> Vec<Line> {
    if w == 0 || children.is_empty() {
        return Vec::new();
    }
    // One pass: lay each non-elastic child once, and take its width from what
    // it produced. `None` marks a spacer, whose width is not known yet.
    let mut columns: Vec<Option<Vec<Line>>> = Vec::with_capacity(children.len());
    let mut widths: Vec<u16> = Vec::with_capacity(children.len());
    let mut used = 0u16;
    let mut spacers = 0usize;
    for child in children {
        match child {
            El::Spacer => {
                spacers += 1;
                columns.push(None);
                widths.push(0);
            }
            other => {
                let room = w.saturating_sub(used);
                let lines = other.lay(room);
                let cw = lines
                    .iter()
                    .map(|l| l.width() as u16)
                    .max()
                    .unwrap_or(0)
                    .min(room);
                used = used.saturating_add(cw);
                columns.push(Some(lines));
                widths.push(cw);
            }
        }
    }

    // Spacers share the slack; the last takes the rounding, so the row ends
    // exactly at `w`.
    let slack = w.saturating_sub(used);
    let mut spent = 0u16;
    let mut seen = 0usize;
    for (i, col) in columns.iter().enumerate() {
        if col.is_none() {
            seen += 1;
            let share = if seen == spacers {
                slack.saturating_sub(spent)
            } else {
                slack / spacers.max(1) as u16
            };
            spent += share;
            widths[i] = share;
        }
    }

    let height = columns
        .iter()
        .map(|c| c.as_ref().map_or(0, Vec::len))
        .max()
        .unwrap_or(0)
        .max(1);
    (0..height)
        .map(|i| {
            let mut row = Line::empty();
            for (col, &cw) in columns.iter().zip(&widths) {
                let piece = col
                    .as_ref()
                    .and_then(|lines| lines.get(i).cloned())
                    .unwrap_or_default();
                for span in pad_to(piece, cw as usize).spans {
                    row.push(span);
                }
            }
            row.truncate(w as usize)
        })
        .collect()
}

fn pad_to(line: Line, cells: usize) -> Line {
    let have = line.width();
    if have >= cells {
        return line.truncate(cells);
    }
    let mut out = line;
    out.push(Span::raw(" ".repeat(cells - have)));
    out
}

/// The border, in one place.
///
/// `overlay::framed` used to own the only box-drawing in the crate; a second
/// copy here would mean modals and panels drift into different corners.
pub fn edge() -> Style {
    Style::new().fg(Color::role(Role::Border))
}

/// A horizontal rule with an optional caption set into it.
/// A full-width rule, no corners.
///
/// What separates the transcript from the composer, and what a turn's summary
/// is set into. `atomcode-tuix` draws both as plain runs of `─` rather than as
/// a box: a box around the composer looks tidy on its own and wrong beside the
/// product people already use.
pub fn plain_rule(w: usize, style: Style) -> Line {
    Line::styled("─".repeat(w), style)
}

/// A rule with text set into the middle of it.
///
/// The turn separator: `───── ✓ 完成 · 2 轮 ─────`. Centred, because it reads as
/// a divider with a label rather than as a line someone wrote.
/// Whether [`captioned_rule`] would set this caption into the rule at width
/// `w`, or fall back to a bare rule. Public so a block can decide to say the
/// same thing another way — under the rule, wrapped — rather than lose it.
pub fn caption_fits(caption: &str, w: usize) -> bool {
    width::str_width(&format!(" {} ", caption.trim())) + 4 <= w
}

pub fn captioned_rule(caption: &str, w: usize, rule_style: Style, text_style: Style) -> Line {
    if !caption_fits(caption, w) {
        return plain_rule(w, rule_style);
    }
    let text = format!(" {} ", caption.trim());
    let tw = width::str_width(&text);
    let left = (w - tw) / 2;
    let right = w - tw - left;
    Line::from_spans(vec![
        Span::styled("─".repeat(left), rule_style),
        Span::styled(text, text_style),
        Span::styled("─".repeat(right), rule_style),
    ])
}

pub fn rule(left: char, right: char, caption: Option<&str>, w: usize) -> Line {
    if w < 2 {
        return Line::styled("─".repeat(w), edge());
    }
    let inner = w - 2;
    let text = caption
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| format!(" {c} "))
        .unwrap_or_default();
    // A caption that does not fit is dropped rather than truncated: half a word
    // on a border reads as damage, and the border still does its job without it.
    let text = if width::str_width(&text) + 2 > inner {
        String::new()
    } else {
        text
    };
    let lead = if text.is_empty() { 0 } else { 1 };
    let tail = inner.saturating_sub(lead + width::str_width(&text)).max(0);
    Line::from_spans(vec![Span::styled(
        format!(
            "{left}{}{text}{}{right}",
            "─".repeat(lead),
            "─".repeat(tail)
        ),
        edge(),
    )])
    .truncate(w)
}

fn frame(title: Option<&str>, footer: Option<&str>, child: &El, w: u16) -> Vec<Line> {
    if w < 4 {
        // Too narrow to be a box. Draw the contents rather than a broken frame:
        // chrome is the first thing to give up when there is no room.
        return child.lay(w);
    }
    let inner = w - 2;
    let mut out = vec![rule('┌', '┐', title, w as usize)];
    let bar = Span::styled("│", edge());
    for line in child.lay(inner) {
        let mut row = Line::from_spans(vec![bar.clone()]);
        let padded = pad_to(line, inner as usize);
        for span in padded.spans {
            row.push(span.clone());
        }
        row.push(bar.clone());
        out.push(row.truncate(w as usize));
    }
    out.push(rule('└', '┘', footer, w as usize));
    out
}

#[cfg(test)]
mod tests {
    //! Slicing is allowed in here: every index is a byte offset a test computed
    //! from its own ASCII fixture, and the point of the assertion is usually
    //! that offset. Production code says why each slice is safe instead; this
    //! is the one place where "the test wrote the string" is the whole reason.
    #![allow(clippy::string_slice, reason = "byte offsets over the test's own fixtures")]

    use super::*;

    fn plain(el: &El, w: u16) -> Vec<String> {
        el.lay(w).iter().map(|l| l.plain()).collect()
    }

    #[test]
    fn a_spacer_flushes_the_second_child_to_the_right_edge() {
        // The thing the status bar hand-rolled.
        let el = El::row(vec![El::raw("left"), El::Spacer, El::raw("right")]);
        let out = plain(&el, 20);
        assert_eq!(out, vec!["left           right"[..20].to_string()]);
        assert_eq!(el.lay(20)[0].width(), 20, "and it fills the width exactly");
    }

    #[test]
    fn two_spacers_centre_what_is_between_them() {
        let el = El::row(vec![El::Spacer, El::raw("mid"), El::Spacer]);
        let out = plain(&el, 11);
        assert_eq!(out, vec!["    mid    ".to_string()]);
    }

    #[test]
    fn nothing_a_row_produces_is_ever_wider_than_the_width() {
        // The invariant the containment check would otherwise catch at paint
        // time, upheld here so a module cannot get it wrong.
        for w in 1u16..40 {
            let el = El::row(vec![
                El::raw("a rather long left side"),
                El::Spacer,
                El::raw("and a long right side"),
            ]);
            for line in el.lay(w) {
                assert!(line.width() <= w as usize, "w={w}: {:?}", line.plain());
            }
        }
    }

    #[test]
    fn a_frame_puts_its_captions_on_the_rules_not_in_the_body() {
        let el = El::framed(El::raw("body")).title("t").footer("hint");
        let out = plain(&el, 20);
        assert_eq!(out.len(), 3, "top, one body row, bottom: {out:?}");
        assert!(out[0].contains("t") && out[0].starts_with('┌'));
        assert!(out[1].starts_with('│') && out[1].contains("body") && out[1].ends_with('│'));
        assert!(out[2].contains("hint") && out[2].starts_with('└'));
        assert!(out.iter().all(|l| width::str_width(l) == 20));
    }

    #[test]
    fn a_caption_that_does_not_fit_is_dropped_rather_than_cut() {
        // Half a word on a border reads as damage.
        let el = El::framed(El::raw("x")).title("a very long panel title indeed");
        let out = plain(&el, 12);
        assert!(!out[0].contains("a very"), "{out:?}");
        assert_eq!(width::str_width(&out[0]), 12);
    }

    #[test]
    fn a_frame_too_narrow_to_draw_gives_up_the_chrome_not_the_content() {
        let el = El::framed(El::raw("hi")).title("t");
        assert_eq!(plain(&el, 3), vec!["hi".to_string()]);
    }

    #[test]
    fn cjk_keeps_the_edges_aligned() {
        let el = El::framed(El::row(vec![El::raw("宽字符"), El::Spacer, El::raw("x")]));
        for line in el.lay(20) {
            assert_eq!(line.width(), 20, "{:?}", line.plain());
        }
    }

    // ---- the block-level pass (moved here with `Region`) ----------------

    fn ids(v: &[(El, Rect)]) -> Vec<String> {
        v.iter()
            .map(|(r, _)| match r {
                El::Stream => "stream".to_string(),
                El::Module(id) => id.clone(),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn a_fixed_split_gives_exactly_what_it_asks_for() {
        let tree = El::split(
            Dir::Vertical,
            Constraint::Cells(3),
            El::view("top"),
            El::view("bottom"),
        );
        let out = tree.layout(Rect::sized(20, 10));
        assert_eq!(ids(&out), vec!["top", "bottom"]);
        assert_eq!(out[0].1, Rect::new(0, 0, 20, 3));
        assert_eq!(out[1].1, Rect::new(0, 3, 20, 7));
    }

    #[test]
    fn fill_leaves_room_for_what_is_below_it() {
        // The shape the TUI actually uses: stream takes what is left.
        let tree = El::split(
            Dir::Vertical,
            Constraint::Fill,
            El::Stream,
            El::split(
                Dir::Vertical,
                Constraint::Cells(1),
                El::view("status"),
                El::view("input"),
            ),
        );
        let out = tree.layout(Rect::sized(40, 12));
        assert_eq!(ids(&out), vec!["stream", "status", "input"]);
        assert_eq!(out[0].1.h + out[1].1.h + out[2].1.h, 12, "no rows lost");
        assert_eq!(out[1].1.h, 1);
    }

    #[test]
    fn an_unmounted_module_collapses_instead_of_panicking() {
        let tree = El::split(
            Dir::Horizontal,
            Constraint::Percent(70),
            El::Stream,
            El::view("findings"),
        );
        let pruned = tree.prune(&|id| id != "findings");
        assert_eq!(pruned, El::Stream, "the split collapses to what is left");
        let out = pruned.layout(Rect::sized(30, 5));
        assert_eq!(out[0].1, Rect::sized(30, 5), "the survivor takes the space");
    }

    #[test]
    fn layout_never_panics_at_any_size() {
        let tree = El::split(
            Dir::Vertical,
            Constraint::Fill,
            El::Stream,
            El::split(
                Dir::Horizontal,
                Constraint::Percent(30),
                El::view("a"),
                El::Stack(vec![El::view("b"), El::view("c")]),
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
        let tree = El::Stack(vec![El::view("under"), El::view("over")]);
        let out = tree.layout(Rect::sized(10, 4));
        assert_eq!(ids(&out), vec!["under", "over"], "later is on top");
        assert_eq!(out[0].1, out[1].1);
    }

    // ---- flex ------------------------------------------------------------

    fn wants_one(_: &str) -> u16 {
        1
    }

    fn placed(el: &El, w: u16, h: u16) -> Vec<(String, Rect)> {
        el.layout_with(Rect::sized(w, h), &wants_one)
            .into_iter()
            .filter_map(|(e, r)| match e {
                El::Module(id) => Some((id, r)),
                El::Stream => Some(("stream".to_string(), r)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn grow_weights_divide_the_leftover() {
        // Three panes, the middle one twice as wide. Two-child splits could
        // only express this by nesting and hand-computed percentages.
        let el = El::flex(
            Dir::Horizontal,
            vec![
                Item {
                    basis: Constraint::Fill,
                    grow: 1,
                    el: El::view("a"),
                },
                Item {
                    basis: Constraint::Fill,
                    grow: 2,
                    el: El::view("b"),
                },
                Item {
                    basis: Constraint::Fill,
                    grow: 1,
                    el: El::view("c"),
                },
            ],
        );
        let out = placed(&el, 43, 10);
        let w: Vec<u16> = out.iter().map(|(_, r)| r.w).collect();
        assert_eq!(
            w.iter().sum::<u16>(),
            43,
            "the row is filled exactly: {w:?}"
        );
        assert!(
            w[1] >= w[0] * 2 - 1 && w[1] <= w[0] * 2 + 2,
            "2:1:1 → {w:?}"
        );
    }

    #[test]
    fn a_fixed_child_keeps_its_size_while_the_rest_grow() {
        let el = El::flex(
            Dir::Vertical,
            vec![
                Item::fixed(1, El::view("status")),
                Item::grow(El::Stream),
                Item::fixed(3, El::view("input")),
            ],
        );
        let out = placed(&el, 80, 24);
        assert_eq!(out[0].1.h, 1);
        assert_eq!(out[1].1.h, 20);
        assert_eq!(out[2].1.h, 3);
        assert_eq!(out[2].1.y, 21, "and they abut with no gap");
    }

    #[test]
    fn gap_puts_space_between_children_and_nowhere_else() {
        let el = El::flex(
            Dir::Vertical,
            vec![
                Item::fixed(2, El::view("a")),
                Item::fixed(2, El::view("b")),
                Item::fixed(2, El::view("c")),
            ],
        )
        .gap(1);
        let out = placed(&el, 10, 10);
        assert_eq!(out[0].1.y, 0);
        assert_eq!(out[1].1.y, 3, "2 rows then a gap");
        assert_eq!(out[2].1.y, 6);
    }

    #[test]
    fn overflow_takes_room_from_the_elastic_children_first() {
        // Asking for more than exists must not panic and must not silently
        // shrink the child that was explicitly sized — it asked not to be.
        let el = El::flex(
            Dir::Vertical,
            vec![
                Item::fixed(4, El::view("pinned")),
                Item::grow(El::view("elastic")),
                Item::fixed(4, El::view("also_pinned")),
            ],
        );
        let out = placed(&el, 10, 6);
        let by = |name: &str| out.iter().find(|(id, _)| id == name).map(|(_, r)| r.h);
        // Squeezed to nothing means not placed at all — there is no rect to
        // draw into, and a zero-height part would only be something for the
        // containment check to complain about later.
        assert_eq!(by("elastic"), None, "the elastic one gives up first");
        assert_eq!(by("pinned"), Some(4), "what was declared first survives");
        assert_eq!(by("also_pinned"), Some(2), "the later fixed child yields");
        assert!(
            out.iter().map(|(_, r)| r.h).sum::<u16>() <= 6,
            "and nothing overflows the area"
        );
    }

    #[test]
    fn flex_never_panics_at_any_size() {
        let el = El::flex(
            Dir::Horizontal,
            vec![
                Item::fixed(3, El::view("a")),
                Item::grow(El::Stream),
                Item {
                    basis: Constraint::Percent(30),
                    grow: 0,
                    el: El::view("b"),
                },
            ],
        )
        .gap(2);
        for w in 0u16..40 {
            for h in [0u16, 1, 5, 24] {
                let _ = el.layout_with(Rect::sized(w, h), &wants_one);
            }
        }
    }

    #[test]
    fn deep_nesting_terminates() {
        // The regression guard for the stack overflow above. Depth is what a
        // component tree grows, so "it terminates" has to be checked at a depth
        // no one would write by hand.
        let mut el = El::raw("leaf");
        for _ in 0..200 {
            el = El::row(vec![El::Spacer, el, El::Spacer]);
        }
        let out = el.lay(40);
        assert_eq!(out.len(), 1);
        assert!(out[0].width() <= 40);
    }

    #[test]
    fn a_column_stacks_and_an_indent_shifts() {
        let el = El::col(vec![
            El::raw("one"),
            El::Indent(2, Box::new(El::raw("two"))),
        ]);
        assert_eq!(plain(&el, 10), vec!["one".to_string(), "  two".to_string()]);
    }

    #[test]
    fn fixed_pads_a_short_child_and_clips_a_long_one() {
        let el = El::row(vec![El::Fixed(6, Box::new(El::raw("ab"))), El::raw("|end")]);
        assert_eq!(plain(&el, 20), vec!["ab    |end".to_string()]);
        let el = El::row(vec![
            El::Fixed(3, Box::new(El::raw("abcdefgh"))),
            El::raw("|"),
        ]);
        assert_eq!(plain(&el, 20), vec!["abc|".to_string()]);
    }
}
