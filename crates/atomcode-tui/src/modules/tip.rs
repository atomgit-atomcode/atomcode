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
//! Nothing folds into a tip, and nothing ever will: a tip is not a fact. What
//! this row draws is [`Moment::notice`](crate::moment::Moment::notice) — a line
//! the host was asked to say for a moment, with the reading it expires at
//! travelling alongside it — and, when nothing is being said, the offer of a
//! picture on the clipboard ([`Moment::clipboard_caption`](crate::moment::Moment::clipboard_caption)),
//! which is the same kind of thing: something the person could do next. So the
//! state below stays empty and every tip is read off the moment, the way the
//! live line reads its clock.

use atomcode_harness::session::SessionEvent;

use crate::el::El;
use crate::frame::Line;
use crate::module::{Height, View};
use crate::moment::Viewport;
use crate::theme::{self, Role};

pub const ID: &str = "tip";

/// What this row has to say, and there is nothing to fold.
///
/// Facts arrive through [`View::absorb`] and are deliberately dropped: nothing
/// in the log is a tip. A tip is about what a person could do next, which is a
/// question about the *moment* rather than about what has happened — so it is
/// read off the viewport and this struct stays empty.
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
    /// a tip lands out of the way of the words being typed, still on screen,
    /// without the row itself needing to know how wide it is. An expired notice
    /// draws the same blank row — the reservation is what is being drawn, and a
    /// row of blanks is not the same thing as no row at all.
    ///
    /// A refusal is drawn in the error role, not the muted one: the tip row is
    /// small and quiet, and "done" and "could not" must not be the same colour
    /// simply because they share a row.
    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let live = vp
            .moment
            .notice
            .as_ref()
            // A `below` notice — the exit hint — is the status line's row, not
            // this one; drawing it here would put it above the box and double it
            // up with the row below.
            .filter(|n| !n.below && n.is_live(vp.moment.now));
        let (text, role) = match live {
            Some(n) if n.refused => (n.text.clone(), Role::Error),
            Some(n) => (n.text.clone(), Role::Muted),
            // Nothing said for a moment: the clipboard's own offer, when a
            // picture has just arrived there. The same row because it is the same
            // kind of thing — something the person could do next, right now — and
            // the right edge because that is out of the way of the words being
            // typed. It is not a notice and does not become one: a paste takes it
            // down, not a clock (`crate::clip_hint`).
            //
            // ponytail: it shows over a panel too, where the key it names does
            // nothing. Gate on the composer being the screen if that bites.
            None => (
                vp.moment.clipboard_caption().unwrap_or_default(),
                Role::Muted,
            ),
        };
        El::row(vec![El::Spacer, El::styled(text, theme::fg(role))]).lay(w)
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
    use crate::moment::{Moment, Timestamp};
    use crate::width;

    crate::tui_conformance!(view Tip as tip_conformance);

    fn draw_at(moment: &Moment, w: u16, h: u16) -> Vec<Line> {
        let vp = Viewport::new(Rect::sized(w, h), moment);
        Tip::render(&State, &vp)
    }

    fn draw(w: u16, h: u16) -> Vec<String> {
        draw_at(&Moment::default(), w, h)
            .iter()
            .map(|l| l.plain())
            .collect()
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

    /// The offer of a picture on the clipboard lands here, against the right
    /// edge, one row above the field — it names the key that takes a picture on
    /// *this* terminal (`ctrl+v`, or `ctrl+alt+v` and `/paste` where the terminal
    /// keeps `ctrl+v` for its own text paste), and nothing on the row below says
    /// which key that is. Moved here from the composer's upper rule: beside the
    /// field it read as chrome about the box, in the same band as the session's
    /// name and competing with it for the one shoulder.
    #[test]
    fn a_picture_on_the_clipboard_is_offered_here_with_the_key_that_takes_it() {
        let mut moment = Moment {
            now: Timestamp::millis(0),
            ..Moment::default()
        };
        assert!(
            draw_at(&moment, 60, 1)[0].plain().trim().is_empty(),
            "nothing offered, nothing said"
        );

        moment.clipboard_hint = true;
        let here = draw_at(&moment, 60, 1);
        assert!(
            here[0].plain().contains("剪贴板有图片") && here[0].plain().contains("ctrl+v"),
            "{:?}",
            here[0].plain()
        );
        assert!(
            here[0].plain().ends_with("粘贴"),
            "against the right edge, above the box: {:?}",
            here[0].plain()
        );

        // Where the terminal keeps `ctrl+v` for itself, the row names the key
        // that does work here and the command for the rest.
        moment.caps.paste_image = crate::caps::PasteImage::CtrlAltVOrCommand;
        let elsewhere = draw_at(&moment, 80, 1);
        assert!(
            elsewhere[0].plain().contains("ctrl+alt+v") && elsewhere[0].plain().contains("/paste"),
            "{:?}",
            elsewhere[0].plain()
        );

        // Something said for a moment outranks it: a notice answers what the
        // person just did, the offer is ambient, and the row holds one line.
        let said = moment.with_notice("已复制到剪贴板", false, Timestamp::millis(0));
        let out = draw_at(&said, 80, 1);
        assert!(
            out[0].plain().contains("已复制到剪贴板") && !out[0].plain().contains("剪贴板有图片"),
            "{:?}",
            out[0].plain()
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
        // `Spacer` eats the slack. Driven with a real notice through `render`
        // rather than laid out by hand, so what is checked is the row that is
        // actually drawn at widths where the arithmetic could go wrong.
        for w in [8u16, 20, 41, 80] {
            let moment = Moment::default().with_notice("已复制", false, Timestamp::millis(0));
            let out = draw_at(&moment, w, 1);
            let line = out.first().expect("one row");
            assert_eq!(width::str_width(&line.plain()), w as usize, "w={w}");
            assert!(
                line.plain().ends_with("已复制"),
                "w={w}: a tip belongs at the right edge, not adrift: {:?}",
                line.plain()
            );
        }
    }

    #[test]
    fn a_notice_is_drawn_until_its_reading_passes_and_blank_after() {
        // The whole point of carrying the expiry with the text: the row says
        // something for a while and then stops, and the *module* decides that
        // from two injected numbers. A module that read a clock would be right
        // here and wrong in the test loop.
        let says = Moment::default().with_notice("已复制到剪贴板", false, Timestamp::millis(1_000));
        assert!(
            draw_at(&says, 40, 1)[0].plain().contains("已复制到剪贴板"),
            "it is drawn while it is live"
        );

        // One reading before the end: still there. The last live reading is the
        // one below the expiry, which is the boundary worth holding.
        let mut last = says.clone();
        last.now = Timestamp::millis(3_999);
        assert!(
            draw_at(&last, 40, 1)[0].plain().contains("已复制到剪贴板"),
            "live right up to the reading before the expiry"
        );

        // At the expiry: gone, and the row is still a row of blanks rather than
        // nothing — the reservation does not come and go with the news.
        let mut off = says;
        off.now = Timestamp::millis(4_000);
        let out = draw_at(&off, 40, 1);
        assert_eq!(out.len(), 1, "the row is still asked for");
        assert!(
            out[0].plain().trim().is_empty(),
            "the tip is gone at its expiry: {:?}",
            out[0].plain()
        );
    }

    #[test]
    fn a_below_notice_is_the_status_lines_and_not_drawn_here() {
        // The exit hint is a notice too, but it belongs below the box. This row
        // is above it and must stay blank so the two never draw at once.
        let mut moment = Moment::default();
        moment.now = Timestamp::millis(0);
        moment.notice = Some(
            crate::moment::Notice::for_ms("再按 Ctrl+C 退出", false, Timestamp::millis(0), 2_000)
                .below(),
        );
        let out = draw_at(&moment, 60, 1);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].plain().trim().is_empty(),
            "a below-notice is the status line's, not the tip row's: {:?}",
            out[0].plain()
        );
    }

    #[test]
    fn a_refusal_is_not_drawn_in_the_colour_of_a_success() {
        // Same row, two meanings, and the row is small enough that the wording
        // is all there is room for. "已复制" and "没有可复制的内容" must not be
        // the same grey.
        let said = Moment::default().with_notice("已复制到剪贴板", false, Timestamp::millis(0));
        let refused = Moment::default().with_notice("没有可复制的内容", true, Timestamp::millis(0));
        let ink = |m: &Moment| {
            let rows = draw_at(m, 40, 1);
            rows[0]
                .spans
                .iter()
                .find(|s| !s.text.trim().is_empty())
                .map(|s| s.style)
                .expect("the words")
        };
        assert_ne!(
            ink(&said),
            ink(&refused),
            "a refusal wears its own role, not the one good news wears"
        );
        assert_eq!(ink(&refused), theme::fg(Role::Error));
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
