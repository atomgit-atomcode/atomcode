//! The tip line: one reserved row above the composer, right-aligned, empty
//! most of the time.
//!
//! It exists so that advice has somewhere to go that is not the status line and
//! not the input box. The status line is a report of where the session is — what
//! model, what directory, how much context — and a tip is none of that. The
//! input box is the thing being typed into, and a hint written beside the caret
//! competes with the words for the same glance.
//!
//! **The row is asked for unconditionally.** Its [`height`](View::height) is
//! `Hug(1)` whatever the state says, which is the whole difference between this
//! module and the live line beside it: the live line hands its row back between
//! turns with `Hug(0)`, so the composer closes up around the field, while this
//! one asks for its row either way. That is deliberate, and it is the one place
//! a permanent blank row is the right answer rather than chrome: a tip that
//! pushed the box down when it appeared would move the field out from under a
//! hand already on the way to it. Here the box never moves, because the row is
//! always the row.
//!
//! It is a row of its own rather than a line inside the input module (which is
//! where it started) for the reason the rest of this crate is built on: one
//! panel, one row, so `[[remove]] id = "tui-panel-tip"` takes it off the screen
//! and nothing else notices — and a third crate can mount its own in the same
//! place without editing the compositor. A row welded into the input module's
//! geometry could not do either, and every edit to what this says would be an
//! edit to the layout arithmetic of the text field.
//!
//! Nothing writes a tip yet. That is a state, not an omission: what this module
//! owes the screen today is the *row*, and the difference between "no tip" and
//! "a tip" is nothing on screen — so the day something does write one, it
//! changes only what this file draws, not where anything sits.

use atomcode_harness::session::SessionEvent;

use crate::el::El;
use crate::frame::Line;
use crate::module::{Height, View};
use crate::moment::Viewport;
use crate::theme::{self, Role};

pub const ID: &str = "tip";

/// What this row has to say, and there is nothing to fold yet.
///
/// Facts arrive through [`View::absorb`] and are deliberately dropped: nothing
/// in the log is a tip. A tip is about what a person could do next, which is a
/// question about the *moment* rather than about what has happened — so if one
/// is ever written, it will be read off the viewport the way the live line reads
/// its clock, and this struct will stay empty.
#[derive(Default)]
pub struct State;

pub struct Tip;

impl View for Tip {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &SessionEvent) {}

    /// One row, right-aligned by construction.
    ///
    /// The `Spacer` eats the slack and the text sits against the right edge, so
    /// whatever this eventually says lands where a tip belongs — out of the way
    /// of the words being typed, still on screen — without the row itself
    /// needing to know how wide it is. Empty today, and an empty row laid out
    /// this way is a row of blanks rather than nothing: the reservation is what
    /// is being drawn.
    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        El::row(vec![
            El::Spacer,
            El::styled(String::new(), theme::fg(Role::Muted)),
        ])
        .lay(w)
    }

    /// Always one row — the point of the module.
    ///
    /// `Hug(1)` and not `Fixed(1)`: both ask for the same row here, but `Hug`
    /// is the honest one, because this row would never want two. And not
    /// `Fill`, which would let a tip grow into whatever the composer had spare
    /// and push the field down as it did.
    ///
    /// The host still arbitrates: on a screen too short to seat the composer as
    /// well, the field takes the row back rather than the box being drawn
    /// without its bottom rule. A module requests; it does not seize. That is
    /// the same bargain the live line's margin strikes, and the reason it is
    /// struck here in `height` rather than defended in `render` is that a
    /// module drawing rows it did not ask for is drawing outside its rect.
    fn height(_state: &State, _moment: &crate::moment::Moment, _width: u16) -> Height {
        Height::Hug(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{Caps, Glyph};
    use crate::frame::Rect;
    use crate::module::{Mounted, ViewObject};
    use crate::moment::Moment;
    use crate::width;

    crate::tui_conformance!(view Tip as tip_conformance);

    fn draw(w: u16, h: u16) -> Vec<String> {
        let moment = Moment::default();
        let vp = Viewport::new(Rect::sized(w, h), &moment);
        Tip::render(&State, &vp).iter().map(|l| l.plain()).collect()
    }

    #[test]
    fn the_row_is_kept_whether_or_not_it_has_anything_to_say() {
        // The property the module exists for. `Hug(0)` is how the live line
        // hands its row back between turns; this row is asked for from the same
        // kind of predicate and deliberately answers the same always — a tip
        // that arrived by pushing the box down would move the field under a
        // hand already reaching for it.
        assert_eq!(Tip::height(&State, &Moment::default(), 80), Height::Hug(1));
        assert_eq!(
            Tip::height(&State, &Moment::default().working(), 80),
            Height::Hug(1),
            "and a turn in flight changes nothing here"
        );
    }

    #[test]
    fn an_empty_row_is_still_a_row_of_the_full_width() {
        // Blank, not absent: `Line::empty()` would be a zero-width line, and
        // the host's containment check reads the *lines* a module returned. A
        // row that drew nothing is one the frame has every right to collapse.
        let out = draw(40, 1);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0], " ".repeat(40), "a row of blanks: {out:?}");
        assert_eq!(out[0].len(), 40);
    }

    #[test]
    fn whatever_it_says_will_land_against_the_right_edge() {
        // The alignment is structural, not a number to get right later: the
        // `Spacer` eats the slack. Checked against the row's own layout, at
        // widths where the arithmetic could go wrong, so that the day a tip is
        // written this test is already holding the edge.
        for w in [8u16, 20, 41, 80] {
            let placed =
                El::row(vec![El::Spacer, El::styled("tip", theme::fg(Role::Muted))]).lay(w);
            let line = placed.first().expect("one row");
            assert_eq!(width::str_width(&line.plain()), w as usize, "w={w}");
            assert!(
                line.plain().ends_with("tip"),
                "w={w}: a tip belongs at the right edge, not adrift: {:?}",
                line.plain()
            );
        }
    }

    #[test]
    fn an_empty_row_draws_no_border_of_its_own() {
        // Nothing here draws a rule, a corner or a prompt: those belong to the
        // field below, and a second set would read as a box around nothing.
        let caps = Caps::default();
        let out = draw(30, 1);
        for glyph in [
            Glyph::Horizontal,
            Glyph::Vertical,
            Glyph::TopLeft,
            Glyph::BottomRight,
        ] {
            assert!(
                !out[0].contains(caps.g(glyph)),
                "{glyph:?} is the field's, not this row's: {out:?}"
            );
        }
    }

    #[test]
    fn it_says_nothing_because_nothing_has_been_said() {
        // The state is a fact about the log and the log holds no tips, so the
        // row is blank on every fact in the corpus rather than on the empty
        // state alone. When something does write a tip this is the test that
        // will have to change, on purpose.
        let mounted = Mounted::<Tip>::new();
        for fact in crate::conformance::facts() {
            mounted.absorb(&fact);
        }
        let moment = Moment::default();
        let vp = Viewport::new(Rect::sized(40, 1), &moment);
        let rows = mounted.render(&vp);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].plain().trim().is_empty(), "{:?}", rows[0].plain());
    }
}
