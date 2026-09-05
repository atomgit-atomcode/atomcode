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

/// A piece of screen.
///
/// Two kinds of node, and the distinction is the same one HTML makes:
///
/// * **块级** — [`El::Split`], [`El::Stack`], [`El::Module`], [`El::Stream`].
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
    /// 块级：two children dividing an area.
    Split {
        dir: Dir,
        at: Constraint,
        a: Box<El>,
        b: Box<El>,
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

    /// 块级：divide an area between two children.
    pub fn split(dir: Dir, at: Constraint, a: El, b: El) -> El {
        El::Split {
            dir,
            at,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    pub fn row(children: Vec<El>) -> El {
        El::Row(children)
    }
    pub fn col(children: Vec<El>) -> El {
        El::Col(children)
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
        El::Split {
            dir: Dir::Vertical,
            at: Constraint::Fill,
            a: Box::new(El::Stream),
            b: Box::new(below),
        }
        .with_second_size(rows)
    }

    fn with_second_size(self, rows: u16) -> El {
        match self {
            El::Split { dir, a, b, .. } => El::Split {
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

    fn flipped(self) -> El {
        match self {
            El::Split { dir, at, a, b } => El::Split {
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
            El::Split { a, b, .. } => {
                a.walk(f);
                b.walk(f);
            }
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
            El::Split { dir, at, a, b } => {
                let a = a.prune(mounted);
                let b = b.prune(mounted);
                match (&a, &b) {
                    (El::Empty, El::Empty) => El::Empty,
                    (El::Empty, _) => b,
                    (_, El::Empty) => a,
                    _ => El::Split {
                        dir: *dir,
                        at: *at,
                        a: Box::new(a),
                        b: Box::new(b),
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
            | El::Framed { .. } => out.push((self.clone(), area)),
            El::Stack(children) => children.iter().for_each(|c| c.place_into(area, wants, out)),
            El::Split { dir, at, a, b } => {
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
                a.place_into(ra, wants, out);
                b.place_into(rb, wants, out);
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
            | El::Framed { .. } => 1,
            El::Stack(c) => c.iter().map(|r| r.wanted(dir, wants)).max().unwrap_or(0),
            El::Split { dir: d, at, a, b } => {
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
            El::Module(_) | El::Stream | El::Split { .. } | El::Stack(_) => Vec::new(),
            El::Text(line) => vec![line.truncate(w as usize)],
            El::Col(children) => children.iter().flat_map(|c| c.lay(w)).collect(),
            El::Row(_) | El::Spacer | El::Fixed(..) => vec![self.lay_row(w)],
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
            El::Framed {
                title,
                footer,
                child,
            } => frame(title.as_deref(), footer.as_deref(), child, w),
        }
    }

    /// One line, children laid left to right with `Spacer`s sharing the slack.
    ///
    /// Every arm either returns or descends into a *strictly smaller* node.
    /// The first version wrapped a leaf as `vec![self]` and then recursed on
    /// it — which never advances, and blew the stack. Termination here is
    /// structural, not a depth counter: a counter is a bandage, a shrinking
    /// argument is a cure.
    fn lay_row(&self, w: u16) -> Line {
        let children: Vec<&El> = match self {
            El::Row(cs) => cs.iter().collect(),
            El::Spacer => return Line::from_spans(vec![Span::raw(" ".repeat(w as usize))]),
            El::Fixed(cells, inner) => {
                let cells = (*cells).min(w);
                return pad_to(inner.lay_row(cells), cells as usize);
            }
            // A leaf, or a block asked to behave like one: it contributes its
            // first line. `Row` is a single line by construction — a band of
            // multi-line children is the region tree's job, one level up.
            other => return other.lay(w).into_iter().next().unwrap_or_default(),
        };
        // Two passes: measure what everything but the spacers wants, then hand
        // the remainder out. A single pass cannot know the slack.
        let mut pieces: Vec<Option<Line>> = Vec::with_capacity(children.len());
        let mut used = 0usize;
        let mut spacers = 0usize;
        for child in &children {
            match child {
                El::Spacer => {
                    spacers += 1;
                    pieces.push(None);
                }
                El::Fixed(cells, inner) => {
                    let line = inner.lay_row(*cells);
                    let padded = pad_to(line, *cells as usize);
                    used += padded.width();
                    pieces.push(Some(padded));
                }
                other => {
                    // Each child gets at most what is still free, so an early
                    // child cannot push a later one off the line entirely.
                    let room = (w as usize).saturating_sub(used);
                    let line = other.lay_row(room.min(w as usize) as u16);
                    used += line.width();
                    pieces.push(Some(line));
                }
            }
        }
        let slack = (w as usize).saturating_sub(used);
        let mut out = Line::empty();
        let mut spent = 0usize;
        let mut seen = 0usize;
        for piece in pieces {
            match piece {
                Some(line) => {
                    for span in line.spans {
                        out.push(span.clone());
                    }
                }
                None => {
                    seen += 1;
                    // The last spacer takes the rounding, so the total is
                    // exactly `slack` and the right edge lands where it should.
                    let share = if seen == spacers {
                        slack.saturating_sub(spent)
                    } else {
                        slack / spacers.max(1)
                    };
                    spent += share;
                    if share > 0 {
                        out.push(Span::raw(" ".repeat(share)));
                    }
                }
            }
        }
        out.truncate(w as usize)
    }
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
    Style::new().fg(Color::Ansi(244))
}

/// A horizontal rule with an optional caption set into it.
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
