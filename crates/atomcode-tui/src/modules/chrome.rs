//! What a working panel is made of: its rules, its box, its search line, its
//! caret.
//!
//! Extracted when the providers panel arrived, because the alternative was two
//! panels that looked almost the same — and "almost" is the failure: a person
//! works both of them with the same hands, and a search box one cell further in
//! on one of them reads as a bug in whichever they saw second. One
//! implementation, so the two cannot drift.
//!
//! Everything here is drawing and nothing here is state. A panel decides *what*
//! rows it has; this decides what a row looks like once it has been decided.

use crate::frame::{Line, Span, Style};
use crate::theme::{self, Role};
use crate::width;

/// Cells a row's own furniture takes: the pointer and its gap.
pub const LEAD: usize = 2;

/// The narrowest the label column is drawn before a value is put right after it.
pub const LABEL_MIN: usize = 8;

/// The widest, so one long label does not push every value off the edge.
pub const LABEL_MAX: usize = 30;

/// Which column the box's left wall stands in.
pub const BORDER_COL: usize = 0;

/// A panel's own top or bottom rule: a straight line, all the way across.
///
/// **No corners.** A frame's pair of them at the left read as a second box
/// around the panel — a `┌` over a `┌`, which says "here is another container"
/// when what it means is "the panel starts here". A rule says that without
/// competing with the one box a panel actually has: its search field's.
///
/// Drawn through `Caps`, so an ASCII terminal gets `-` rather than `─`.
pub fn panel_edge(w: usize, caps: crate::caps::Caps) -> Line {
    use crate::caps::Glyph;
    if w == 0 {
        return Line::empty();
    }
    Line::styled(caps.g(Glyph::Horizontal).repeat(w), theme::fg(Role::Border)).truncate(w)
}

/// The search box's top or bottom edge.
pub fn box_edge(w: usize, caps: crate::caps::Caps, top: bool) -> Line {
    use crate::caps::Glyph;
    if w == 0 {
        return Line::empty();
    }
    // Narrower than a frame is not a frame: below this there is no room for two
    // corners and a run between them, and half a box reads as damage. A plain
    // rule instead, which still reads as "a box is here, it just does not fit".
    if w < 4 {
        return Line::styled(caps.g(Glyph::Horizontal).repeat(w), theme::fg(Role::Border))
            .truncate(w);
    }
    let (left, right) = match top {
        true => (Glyph::TopLeft, Glyph::TopRight),
        false => (Glyph::BottomLeft, Glyph::BottomRight),
    };
    let run = w.saturating_sub(2 + BORDER_COL);
    let mut spans = vec![Span::styled(" ".repeat(BORDER_COL), Style::new())];
    spans.push(Span::styled(
        format!("{}{}", caps.g(left), caps.g(Glyph::Horizontal).repeat(run)),
        theme::fg(Role::Border),
    ));
    spans.push(Span::styled(
        caps.g(right).to_string(),
        theme::fg(Role::Border),
    ));
    Line::from_spans(spans).truncate(w)
}

/// The search box's text, with a caret while the box has the keyboard.
///
/// No placeholder caption. The box is drawn as a box and the caret is in it,
/// which says "type here" better than a sentence about a key.
pub fn search_line(query: &str, caret: Option<usize>, w: usize, caps: crate::caps::Caps) -> Line {
    use crate::caps::Glyph;
    if w == 0 {
        return Line::empty();
    }
    // Too narrow for a frame: the text stands alone rather than behind a
    // one-cell wall that would eat it.
    let (lead, base) = if w < 4 {
        (String::new(), theme::fg(Role::Muted))
    } else {
        (
            format!("{} ", caps.g(Glyph::Vertical)),
            theme::fg(Role::Warning),
        )
    };
    let mut spans = vec![Span::styled(lead, theme::fg(Role::Border))];
    match caret {
        Some(at) => spans.extend(caret_spans(query, at, base, w.saturating_sub(2))),
        None => spans.push(Span::styled(query.to_string(), base)),
    }
    Line::from_spans(spans).truncate(w)
}

/// The text being typed into a row, with its caret.
///
/// The label stays on the left, so the value being typed is still named: a field
/// that took the whole row would leave the person looking at a number with
/// nothing saying what it is a number *of*.
pub fn edit_line(
    label: &str,
    value: &str,
    caret: usize,
    w: usize,
    caps: crate::caps::Caps,
) -> Line {
    let base = theme::fg(Role::Warning);
    let label_room = w
        .saturating_sub(LEAD)
        .saturating_sub(2)
        .clamp(LABEL_MIN, LABEL_MAX);
    let label = width::take_width(label, label_room);
    let pad = label_room.saturating_sub(width::str_width(&label));
    let mut spans = vec![
        Span::styled(format!("{} ", caps.g(crate::caps::Glyph::Pointer)), base),
        Span::styled(label, base),
        Span::styled(" ".repeat(pad + 2), base),
    ];
    // The field gets what is left after the label, and the caret is drawn inside
    // that — a value longer than the room scrolls off the end rather than
    // wrapping, which is what every field on a terminal does.
    let room = w.saturating_sub(LEAD + label_room + 2);
    spans.extend(caret_spans(value, caret, base, room));
    Line::from_spans(spans).truncate(w)
}

/// `text` split around `at`, with a block where the next character goes.
///
/// The caret is drawn *over* the character at `at` rather than before it, which
/// is what a terminal caret does and what makes the end of a line work: there is
/// no character to sit on, so a block is appended instead and both cases read
/// the same.
#[allow(
    clippy::string_slice,
    reason = "`at` is snapped to `is_char_boundary` just above"
)]
pub fn caret_spans(text: &str, at: usize, base: Style, room: usize) -> Vec<Span> {
    let at = at.min(text.len());
    // Snap to a character boundary: a byte offset from the middle of a
    // multi-byte character would panic on the slice, and every path that moves
    // the caret already keeps it on a boundary — this is the belt to that
    // braces, on the one function that slices.
    let at = (0..=at)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    let before = &text[..at];
    let rest = &text[at..];
    let mut slots = rest.chars();
    let on = slots.next();
    let after: String = slots.collect();
    let caret = Style::new().reverse();
    let mut spans = vec![Span::styled(before.to_string(), base)];
    match on {
        Some(c) => spans.push(Span::styled(c.to_string(), caret)),
        None => spans.push(Span::styled(" ", caret)),
    }
    spans.push(Span::styled(after, base));
    // The caret's own cell comes out of the room, so a full-width line still
    // has somewhere to put it.
    let _ = room;
    spans
}

/// A line padded out to `w` in `style`, so a highlight covers the whole row
/// rather than stopping where the text does.
pub fn pad_to(line: Line, w: usize, style: Style) -> Line {
    let used = line.width();
    if used >= w {
        return line.truncate(w);
    }
    let mut spans = line.spans;
    spans.push(Span::styled(" ".repeat(w - used), style));
    Line::from_spans(spans).truncate(w)
}

/// A panel's header: its name, then its pages, on one row.
///
/// Returns the spans **and** where each page sits in columns, because a pointer
/// has to find the tab that was drawn. Two computations — one for the spans, one
/// for the ranges — would agree until a label changed length, and then a click
/// would switch to the tab next to the one under the pointer.
///
/// The ranges do **not** depend on which page is showing: lighting a tab changes
/// its style, not its width or its place. A hit test that took the current page
/// would be a range that moved under a pointer that had not.
pub fn header_parts(
    name: &str,
    labels: &[&str],
    at: usize,
) -> (Vec<Span>, Vec<(usize, usize, usize)>) {
    // (text, style, the page this cell belongs to — the padding included, so a
    // click on the space beside a label takes that label's page rather than the
    // one it abuts).
    let mut pieces: Vec<(String, Style, Option<usize>)> = vec![
        // Two cells in, which is where every other thing a panel says starts:
        // the box's text sits after its `│ ` and a row's label after its
        // pointer. A title one cell further left than everything under it reads
        // as a mistake.
        ("  ".to_string(), Style::new(), None),
        (name.to_string(), theme::fg(Role::Brand).bold(), None),
        ("   ".to_string(), Style::new(), None),
    ];
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            pieces.push(("  ".to_string(), Style::new(), None));
        }
        let style = if i == at {
            theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
        } else {
            theme::fg(Role::Muted)
        };
        // The pad goes *inside* the highlight, so the lit tab is a band rather
        // than a patch behind its letters — the same choice the answer rows make.
        pieces.push((format!(" {label} "), style, Some(i)));
    }

    let mut spans = Vec::with_capacity(pieces.len());
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    let mut col = 0usize;
    for (text, style, owner) in pieces {
        let w = width::str_width(&text);
        if let Some(page) = owner {
            ranges.push((page, col, col + w));
        }
        col += w;
        spans.push(Span::styled(text, style));
    }
    (spans, ranges)
}

/// Which page is under this cell of a header row.
///
/// `col` is measured from the panel's left edge, which is what the host knows
/// and what a click carries.
pub fn tab_at(name: &str, labels: &[&str], col: usize) -> Option<usize> {
    header_parts(name, labels, 0)
        .1
        .into_iter()
        .find(|(_, from, to)| (*from..*to).contains(&col))
        .map(|(page, _, _)| page)
}
