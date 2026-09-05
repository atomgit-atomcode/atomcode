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

use crate::frame::{Color, Line, Span, Style};
use crate::width;

/// A piece of screen, before it knows how wide it is.
#[derive(Clone, Debug, PartialEq)]
pub enum El {
    /// Nothing. Renders to no lines, takes no space in a `Row`.
    Empty,
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

    /// Render at this width. Every line comes back at most `w` cells wide —
    /// the containment invariant is upheld here rather than by each caller
    /// remembering to truncate.
    pub fn lay(&self, w: u16) -> Vec<Line> {
        if w == 0 {
            return Vec::new();
        }
        match self {
            El::Empty => Vec::new(),
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
