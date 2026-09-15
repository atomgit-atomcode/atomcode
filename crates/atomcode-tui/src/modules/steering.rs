//! What you said while the model was still working, and has not been sent yet.
//!
//! A message typed during a turn does not start a second turn and does not
//! appear in the log when you press enter. It goes to the agent's inbox, and the
//! loop folds it in **at the next round boundary** — when the step in flight
//! finishes and the model is asked again. Until then the fact the transcript
//! folds does not exist, so the words are nowhere on screen: not in the
//! conversation (nothing has been committed), and not in the composer (you
//! already sent them). That gap is what this panel fills.
//!
//! Measured, because the first answer written here was wrong: the text lands at
//! the *round* boundary, not at the end of the turn. A turn with three steps
//! shows the steering line several seconds early, while a tool is still running.
//!
//! **It says "sent to the model" as a fact about your words, and nothing else.**
//! The panel is up exactly while the person has said something and the model has
//! not been handed it — so it goes away at the moment the model gets it, which
//! is when [`AgentEvent::Steered`](atomcode_kernel::event::AgentEvent::Steered)
//! arrives. Keeping it until the turn ended would show the same sentence twice,
//! because the transcript draws the folded `UserMessage` from that same
//! boundary onward.
//!
//! Where the text lives, and why not here: [`Moment::steering`]. Nothing in the
//! log is a steering line — that is the whole reason this panel exists — so it
//! is state about *now*, which is `Moment`'s job (the same reason `notice` and
//! `input` live there). The front end collects it on submit and clears it on
//! confirmation, both through the host, so the row's coming and going is pinned
//! against the reader's scroll like every other tail row.
//!
//! [`Moment::steering`]: crate::moment::Moment::steering

use atomcode_harness::session::SessionEvent;

use crate::el::El;
use crate::frame::Line;
use crate::module::{Height, View};
use crate::moment::{Moment, Viewport};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "steering";

/// Nothing folds: the log has no steering lines by construction.
///
/// See the module docs — the text is in [`Moment`] because it is true of now and
/// is not a fact. Every other view module's state comes from `absorb`; this one
/// has nothing to fold, like `tip`.
#[derive(Default)]
pub struct State;

pub struct Steering;

impl View for Steering {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &SessionEvent) {}

    /// A margin, then the label and the words.
    ///
    /// The words are drawn in the *muted* role rather than the bar a settled
    /// `UserSaid` wears, and that is the honest distinction rather than a
    /// stylistic one: this has not been said into the conversation yet, and a
    /// full-width bar is exactly how the transcript marks the messages that
    /// have. When it is folded in, the block that appears a moment later wears
    /// the bar — the change of appearance is the change of state.
    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let Some(words) = showing(vp.moment) else {
            return Vec::new();
        };

        // The margin, only when the rect can seat it: the same bargain `live`
        // and `todo` strike. Above only — under the words is the pane's own
        // bottom edge, which the read-cursor badge is anchored to.
        let margin = if vp.rect.h >= ROWS {
            MARGIN as usize
        } else {
            0
        };
        let mut out: Vec<Line> = Vec::with_capacity(ROWS as usize);
        for _ in 0..margin {
            out.push(Line::empty());
        }

        // One line per message, oldest first, and the last one is what the
        // reader just sent — so the panel is read top-down like the conversation
        // it is waiting to join. Wrapped, because a long sentence must not be
        // quietly cut: this is the person's own words.
        let label = theme::fg(Role::Muted);
        for text in words.lines() {
            let mut row: Vec<El> = vec![El::styled(
                format!("{} ", vp.moment.caps.g(crate::caps::Glyph::Pending)),
                label,
            )];
            for (i, piece) in wrapped(text, w as usize, 2).into_iter().enumerate() {
                if i > 0 {
                    out.extend(El::row(std::mem::take(&mut row)).lay(w));
                }
                row.push(El::styled(piece, theme::fg(Role::Secondary)));
            }
            out.extend(El::row(row).lay(w));
        }
        out.truncate(vp.rect.h as usize);
        out
    }

    /// The margin and the words, and nothing between turns.
    fn height(_state: &State, moment: &Moment, _width: u16) -> Height {
        match showing(moment) {
            Some(text) => {
                let lines = text.lines().count().max(1);
                Height::Hug((lines + MARGIN as usize).min(u16::MAX as usize) as u16)
            }
            None => Height::Hug(0),
        }
    }
}

/// What this row is showing, or `None` when there is nothing waiting.
///
/// One question, asked by both `render` and `height`: the two disagreeing is
/// either a blank row of chrome above the composer or words drawn outside the
/// rect nothing was asked for. Same shape as `live::showing`.
fn showing(moment: &Moment) -> Option<&str> {
    if moment.steering.is_empty() {
        None
    } else {
        Some(moment.steering.as_str())
    }
}

/// The blank row above the label — the padding that keeps the panel off the live
/// line it sits under.
///
/// Above only, and asked for here for the reason the live line and the task
/// list give: a `gap` in the layout is counted between its children whether or
/// not this row is mounted, so between turns the composer would stand on a
/// blank row. Asked for here, it arrives and leaves with the words.
const MARGIN: u16 = 1;

/// What the row asks for at least: the margin and one line of words.
const ROWS: u16 = 1 + MARGIN;

/// Split `text` into at most `width` cells per line, keeping the marker column.
///
/// Not `crate::content::wrapped`: that one carries a block's own lead and styles
/// for the transcript, and this is a view module drawing its own two-column row.
/// What it shares is the only part that matters — the width is counted in cells,
/// not bytes, so a Chinese sentence wraps where it looks like it should.
fn wrapped(text: &str, width: usize, indent: usize) -> Vec<String> {
    let room = width.saturating_sub(indent).max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    for word in text.split_inclusive(' ') {
        let w = width::str_width(word);
        if used > 0 && used + w > room {
            out.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push_str(word);
        used += w;
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::module::{Mounted, ViewObject};
    use crate::moment::{Activity, Moment};

    crate::tui_conformance!(view Steering as steering_conformance);

    fn draw_at(moment: &Moment, w: u16, h: u16) -> Vec<Line> {
        let vp = Viewport::new(Rect::sized(w, h), moment);
        Steering::render(&State, &vp)
    }

    fn draw(moment: &Moment, w: u16, h: u16) -> Vec<String> {
        draw_at(moment, w, h)
            .iter()
            .map(|l| l.plain().trim_end().to_string())
            .collect()
    }

    fn waiting(text: &str) -> Moment {
        Moment {
            steering: text.to_string(),
            ..Moment::default()
        }
        .working()
    }

    #[test]
    fn nothing_waiting_is_not_a_panel() {
        // The row it fills is the composer's to keep empty between turns; a
        // panel that sat there saying "nothing queued" would be the chrome this
        // avoids.
        assert_eq!(
            draw(&Moment::default().working(), 40, 6),
            Vec::<String>::new()
        );
        assert_eq!(
            Steering::height(&State, &Moment::default().working(), 80),
            Height::Hug(0)
        );
    }

    #[test]
    fn words_waiting_draw_a_margin_then_the_line() {
        // The margin is the module's own row — asked for in `height`, drawn in
        // `render` — so it arrives and leaves with the words rather than being a
        // permanent gap in the layout.
        let m = waiting("and also this");
        let lines = draw(&m, 60, 6);
        assert_eq!(lines.len(), 2, "{lines:#?}");
        assert_eq!(lines[0], "", "the margin comes first: {lines:#?}");
        assert!(
            lines[1].contains("and also this"),
            "the words are the row: {lines:#?}"
        );
        assert_eq!(Steering::height(&State, &m, 60), Height::Hug(2));
    }

    #[test]
    fn a_rect_that_cannot_seat_the_margin_keeps_the_words() {
        // Same bargain as the live line: at a height that cannot hold both, the
        // words win, because a blank row drawn in place of them is a panel that
        // says nothing.
        let m = waiting("and also this");
        let lines = draw(&m, 60, 1);
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(lines[0].contains("and also this"), "{lines:#?}");
    }

    #[test]
    fn several_messages_are_all_shown_oldest_first() {
        // `add_steering` joins them with newlines while the turn runs, so the
        // panel is what says "both of these are on their way" rather than only
        // the last one.
        let m = waiting("first follow-up\nsecond follow-up");
        let lines = draw(&m, 60, 8);
        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert_eq!(lines[0], "");
        assert!(lines[1].contains("first follow-up"), "{lines:#?}");
        assert!(lines[2].contains("second follow-up"), "{lines:#?}");
        assert_eq!(Steering::height(&State, &m, 60), Height::Hug(3));
    }

    #[test]
    fn it_is_muted_rather_than_drawn_as_a_settled_message() {
        // The bar is how the transcript marks a message that HAS been said into
        // the conversation. This has not been, so it must not wear it: the
        // change of appearance when it lands is the signal that it landed.
        let m = waiting("and also this");
        let rows = draw_at(&m, 60, 6);
        let words = rows
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.text.contains("and also this"))
            .expect("the words");
        assert_eq!(
            words.style,
            theme::fg(Role::Secondary),
            "quiet ink, not the settled bar"
        );
    }

    #[test]
    fn the_state_stays_empty_because_no_fact_is_a_steering_line() {
        // Same shape as `tip`: the log has no such fact until the round
        // boundary, so nothing in the corpus can move this row. When something
        // does fold here this is the test that has to change, on purpose.
        let mounted = Mounted::<Steering>::new();
        for fact in crate::conformance::facts() {
            mounted.absorb(&fact);
        }
        let m = Moment::default().working();
        let vp = Viewport::new(Rect::sized(40, 6), &m);
        assert!(
            mounted.render(&vp).is_empty(),
            "a folded fact must not conjure a steering line"
        );
        assert_eq!(mounted.height(&m, 40), Height::Hug(0));
    }

    #[test]
    fn it_asks_for_nothing_once_the_model_has_the_words() {
        // The panel's whole contract: up while the person has said something the
        // model has not seen, gone the moment it has. Cleared, the row is handed
        // straight back — and the activity does not matter, which is the
        // difference from `live`.
        let idle = Moment {
            steering: String::new(),
            ..Moment::default()
        };
        assert_eq!(Steering::height(&State, &idle, 80), Height::Hug(0));
        let working = Moment {
            steering: String::new(),
            activity: Activity::Working,
            ..Moment::default()
        };
        assert_eq!(
            Steering::height(&State, &working, 80),
            Height::Hug(0),
            "a turn in flight with nothing waiting is still not a panel"
        );
    }
}
