//! Layer two: components with state but no domain.
//!
//! Between the drawing primitives and the modules that know what a session is,
//! there is a layer that knows what a *list* is. Six of the sixteen modals in
//! `atomcode-tuix` are a list with a selection and a filter; three are a
//! scrollable body; four are a form of labelled fields; and every single one
//! has a key legend on its bottom rule. That is where the reuse actually lives:
//! the domain components above are each used once, these are used four to six
//! times apiece.
//!
//! # State is an argument, never a field
//!
//! `list(items, selected, …)` — the caller says which row is selected; the list
//! does not remember. This is the same rule that rules out React's hooks here,
//! and it is load-bearing rather than stylistic: the screen is a fold over the
//! session log, so replaying the log has to reproduce the screen cell for cell.
//! A component holding private state breaks that **silently** — the tests stay
//! green and stop meaning anything.
//!
//! # No domain, no operating system
//!
//! Nothing here mentions an agent, a session, a tool or a model; nothing here
//! writes a box-drawing character or reads an environment variable. Both are
//! checked by `gates/tui-layers.sh` rather than left to whoever edits next.

use crate::caps::{Caps, Glyph};
use crate::el::El;
use crate::frame::{Line, Span, Style};
use crate::theme::{self, Role};
use crate::width;

fn dim(_caps: Caps) -> Style {
    theme::fg(Role::Muted)
}
fn accent(_caps: Caps) -> Style {
    theme::fg(Role::Accent)
}

/// One row of a [`list`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Choice {
    pub label: String,
    /// Shown after the label, dimmed. Empty for none.
    pub detail: String,
}

impl Choice {
    pub fn new(label: impl Into<String>) -> Choice {
        Choice {
            label: label.into(),
            detail: String::new(),
        }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Choice {
        self.detail = d.into();
        self
    }
}

/// The window of a long list that should be on screen.
///
/// Split out because it is the part that is easy to get wrong and worth
/// checking on its own: the selected row must always be inside the window, and
/// the window must never run past either end.
pub fn window(len: usize, selected: usize, height: usize) -> (usize, usize) {
    if height == 0 || len == 0 {
        return (0, 0);
    }
    if len <= height {
        return (0, len);
    }
    // Keep a row of context either side where there is room, so moving the
    // selection does not feel like it is stuck to the edge.
    let margin = if height >= 5 { 1 } else { 0 };
    let start = selected
        .saturating_sub(margin)
        .min(len.saturating_sub(height));
    (start, start + height)
}

/// A list with one row selected.
///
/// `height` is how many rows there is room for; the list shows a window around
/// the selection and says how much is off screen, because a list that silently
/// hides most of itself is worse than one that scrolls.
pub fn list(items: &[Choice], selected: usize, height: u16, caps: Caps) -> El {
    if items.is_empty() {
        return El::styled("（空）", dim(caps));
    }
    let h = height as usize;
    let (start, end) = window(items.len(), selected.min(items.len() - 1), h.max(1));
    let hidden = items.len() - (end - start);
    let body_rows = if hidden > 0 { h.saturating_sub(1) } else { h };
    let (start, end) = window(items.len(), selected.min(items.len() - 1), body_rows.max(1));

    let mut rows: Vec<El> = Vec::new();
    for (i, item) in items.iter().enumerate().take(end).skip(start) {
        let picked = i == selected;
        let marker = if picked {
            Span::styled(format!("{} ", caps.g(Glyph::Pointer)), accent(caps))
        } else {
            Span::raw("  ")
        };
        let label = if picked {
            Span::styled(item.label.clone(), accent(caps).bold())
        } else {
            Span::raw(item.label.clone())
        };
        let mut spans = vec![marker, label];
        if !item.detail.is_empty() {
            spans.push(Span::styled(format!("  {}", item.detail), dim(caps)));
        }
        rows.push(El::text(Line::from_spans(spans)));
    }
    let off = items.len() - (end - start);
    if off > 0 {
        rows.push(El::styled(format!("  …还有 {off} 项"), dim(caps)));
    }
    El::col(rows)
}

/// A body taller than its room, with the part that is showing marked.
///
/// The bar is not decoration: without it, a viewport that happens to start
/// mid-document looks like a document that starts there.
pub fn scroll(body: &[Line], offset: usize, height: u16, caps: Caps) -> El {
    let h = height as usize;
    if h == 0 {
        return El::Empty;
    }
    if body.len() <= h {
        return El::col(body.iter().cloned().map(El::text).collect());
    }
    let offset = offset.min(body.len() - h);
    let visible: Vec<El> = body[offset..offset + h]
        .iter()
        .cloned()
        .map(El::text)
        .collect();

    // Thumb: proportional, at least one row, positioned by how far down we are.
    let thumb = ((h * h) / body.len()).max(1);
    let travel = h - thumb;
    let top = if body.len() == h {
        0
    } else {
        (offset * travel) / (body.len() - h)
    };
    let bar: Vec<El> = (0..h)
        .map(|i| {
            let inside = i >= top && i < top + thumb;
            El::styled(
                caps.g(if inside { Glyph::Thumb } else { Glyph::Track }),
                if inside { accent(caps) } else { dim(caps) },
            )
        })
        .collect();

    El::row(vec![
        El::col(visible),
        El::raw(" "),
        El::Fixed(1, Box::new(El::col(bar))),
    ])
}

/// One labelled value in a [`form`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Field {
    pub label: String,
    pub value: String,
    /// Shown dimmed after the value — units, a default, a warning.
    pub hint: String,
}

impl Field {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Field {
        Field {
            label: label.into(),
            value: value.into(),
            hint: String::new(),
        }
    }
    pub fn hint(mut self, h: impl Into<String>) -> Field {
        self.hint = h.into();
        self
    }
}

/// Labelled values in a column, labels aligned.
///
/// The alignment is why this is a component rather than three lines at each
/// call site: the label column is as wide as the widest label, and computing
/// that per screen is exactly the arithmetic that goes wrong once and is never
/// noticed again.
pub fn form(fields: &[Field], focused: usize, caps: Caps) -> El {
    let label_w = fields
        .iter()
        .map(|f| width::str_width(&f.label))
        .max()
        .unwrap_or(0) as u16;
    El::col(
        fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let here = i == focused;
                let marker = if here {
                    Span::styled(format!("{} ", caps.g(Glyph::Pointer)), accent(caps))
                } else {
                    Span::raw("  ")
                };
                let mut spans = vec![Span::styled(
                    f.value.clone(),
                    if here {
                        accent(caps).bold()
                    } else {
                        Style::new()
                    },
                )];
                if !f.hint.is_empty() {
                    spans.push(Span::styled(format!("  {}", f.hint), dim(caps)));
                }
                El::row(vec![
                    El::text(Line::from_spans(vec![marker])),
                    El::Fixed(
                        label_w + 2,
                        Box::new(El::styled(f.label.clone(), dim(caps))),
                    ),
                    El::text(Line::from_spans(spans)),
                ])
            })
            .collect(),
    )
}

/// The key legend that belongs on a panel's bottom rule.
///
/// A string rather than an `El` because that is where it goes: `El::footer`
/// takes text, and a legend rendered as its own line reads as something the
/// program said rather than as what it is.
pub fn keys(pairs: &[(&str, &str)], caps: Caps) -> String {
    pairs
        .iter()
        .map(|(k, what)| format!("{k} {what}"))
        .collect::<Vec<_>>()
        .join(&format!(" {} ", caps.g(Glyph::Separator)))
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

    fn choices(n: usize) -> Vec<Choice> {
        (0..n).map(|i| Choice::new(format!("item {i}"))).collect()
    }

    // ---- the window is the part that is easy to get wrong ----------------

    #[test]
    fn a_short_list_shows_all_of_itself() {
        assert_eq!(window(3, 0, 10), (0, 3));
        assert_eq!(window(3, 2, 10), (0, 3));
    }

    #[test]
    fn the_selection_is_always_inside_the_window() {
        // Exhaustive over a range rather than a few cases: an off-by-one here
        // shows up as "the highlight vanished", which a person reports as the
        // list being broken.
        for len in 1usize..40 {
            for height in 1usize..12 {
                for selected in 0..len {
                    let (a, b) = window(len, selected, height);
                    assert!(
                        a <= selected && selected < b,
                        "len={len} h={height} sel={selected} → {a}..{b}"
                    );
                    assert!(b <= len, "window runs past the end");
                    assert!(b - a <= height, "window is taller than the room");
                }
            }
        }
    }

    #[test]
    fn a_long_list_says_how_much_is_hidden() {
        let out = plain(&list(&choices(50), 0, 5, Caps::default()), 40);
        assert!(out.len() <= 5, "{out:?}");
        assert!(
            out.last().unwrap().contains("还有"),
            "a list that silently hides itself is worse than one that scrolls: {out:?}"
        );
    }

    #[test]
    fn the_selected_row_is_marked_and_only_that_one() {
        let caps = Caps::default();
        let out = plain(&list(&choices(4), 2, 10, caps), 40);
        let marked: Vec<&String> = out
            .iter()
            .filter(|l| l.starts_with(caps.g(Glyph::Pointer)))
            .collect();
        assert_eq!(marked.len(), 1, "{out:?}");
        assert!(marked[0].contains("item 2"));
    }

    #[test]
    fn an_empty_list_says_so_rather_than_drawing_nothing() {
        assert_eq!(
            plain(&list(&[], 0, 5, Caps::default()), 20),
            vec!["（空）".to_string()]
        );
    }

    // ---- scroll ----------------------------------------------------------

    #[test]
    fn a_body_that_fits_gets_no_bar() {
        let body: Vec<Line> = (0..3).map(|i| Line::raw(format!("row {i}"))).collect();
        let caps = Caps::default();
        let out = plain(&scroll(&body, 0, 10, caps), 20);
        assert_eq!(out.len(), 3);
        for g in [Glyph::Track, Glyph::Thumb] {
            assert!(!out[0].contains(caps.g(g)), "{out:?}");
        }
    }

    #[test]
    fn the_thumb_moves_from_top_to_bottom_as_the_offset_does() {
        let caps = Caps::default();
        let body: Vec<Line> = (0..100).map(|i| Line::raw(format!("row {i}"))).collect();
        let thumb_row = |offset: usize| {
            plain(&scroll(&body, offset, 10, caps), 20)
                .iter()
                .position(|l| l.contains(caps.g(Glyph::Thumb)))
                .expect("a thumb somewhere")
        };
        assert_eq!(thumb_row(0), 0, "at the top when at the top");
        assert!(thumb_row(90) >= 8, "and at the bottom when at the bottom");
        assert!(thumb_row(45) > 0 && thumb_row(45) < 9, "and in between");
    }

    #[test]
    fn scrolling_past_the_end_clamps_instead_of_panicking() {
        let body: Vec<Line> = (0..20).map(|i| Line::raw(format!("row {i}"))).collect();
        for offset in [0usize, 15, 19, 200, usize::MAX] {
            let _ = scroll(&body, offset, 5, Caps::default()).lay(20);
        }
    }

    // ---- form ------------------------------------------------------------

    #[test]
    fn labels_line_up_however_long_they_are() {
        let fields = vec![
            Field::new("a", "1"),
            Field::new("a much longer label", "2"),
            Field::new("mid", "3"),
        ];
        let out = plain(&form(&fields, 0, Caps::default()), 60);
        // Display columns, not byte offsets: the focus marker is three bytes
        // and one column, and measuring the wrong one is the very mistake this
        // component exists to stop call sites making.
        let value_col: Vec<usize> = out
            .iter()
            .map(|l| {
                let at = l.find(|c: char| c.is_ascii_digit()).unwrap();
                width::str_width(&l[..at])
            })
            .collect();
        assert!(
            value_col.windows(2).all(|w| w[0] == w[1]),
            "values must start in one column: {out:?}"
        );
    }

    #[test]
    fn exactly_one_field_is_marked_as_focused() {
        let caps = Caps::default();
        let fields = vec![Field::new("a", "1"), Field::new("b", "2")];
        let out = plain(&form(&fields, 1, caps), 40);
        assert_eq!(
            out.iter()
                .filter(|l| l.starts_with(caps.g(Glyph::Pointer)))
                .count(),
            1
        );
        assert!(out[1].starts_with(caps.g(Glyph::Pointer)), "{out:?}");
    }

    // ---- keyhint ---------------------------------------------------------

    #[test]
    fn the_legend_reads_as_one_line_of_chrome() {
        let out = keys(
            &[("↑↓", "选择"), ("⏎", "确认"), ("esc", "取消")],
            Caps::default(),
        );
        assert_eq!(out, "↑↓ 选择 · ⏎ 确认 · esc 取消");
    }

    #[test]
    fn the_legend_degrades_with_the_terminal() {
        // The separator comes from the glyph set, so a plain terminal gets a
        // legend rather than a row of tofu.
        let out = keys(&[("esc", "取消")], Caps::plain());
        assert!(!out.contains('·'), "{out}");
    }

    // ---- the property every widget shares --------------------------------

    #[test]
    fn nothing_any_widget_draws_is_wider_than_its_room() {
        let caps = Caps::default();
        let body: Vec<Line> = (0..30)
            .map(|i| Line::raw(format!("row {i} 中文也算")))
            .collect();
        let fields = vec![Field::new("很长的标签名", "值").hint("提示")];
        for w in 1u16..50 {
            for el in [
                list(&choices(30), 7, 6, caps),
                scroll(&body, 5, 6, caps),
                form(&fields, 0, caps),
            ] {
                for line in el.lay(w) {
                    assert!(line.width() <= w as usize, "w={w}: {:?}", line.plain());
                }
            }
        }
    }
}
