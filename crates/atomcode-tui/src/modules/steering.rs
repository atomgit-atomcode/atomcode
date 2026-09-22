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
//! # Rendering: a compact queue preview
//!
//! Not a stack of transcript look-alikes. A header says what these lines ARE —
//! typed ahead while a turn runs, folded in at the next tool-call boundary (or
//! flushed now with Esc) — and each waiting message is one compact `↳ <text>`
//! preview row, oldest first. So several lines typed ahead read as ONE pending
//! batch rather than N sent-looking bars; the panel stays small even with a
//! handful queued, and a long line is a single truncated preview (the full text
//! is what actually gets sent at the boundary).
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
use crate::i18n::{t, Msg};
use crate::module::{Height, View};
use crate::moment::{Moment, Viewport};
use crate::theme::{self, Role};

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

    /// A margin, then one settled bar per message waiting.
    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let mut body = bars(vp.moment, w);
        if body.is_empty() {
            return Vec::new();
        }

        // The margin, only when the rect can seat it *as well as* the bars: the
        // same bargain `live` and `todo` strike. Drawn first and dropped first,
        // so at a height that cannot hold both the words win — a blank row in
        // place of what the person typed is a panel that says nothing.
        let margin = if vp.rect.h as usize >= body.len() + MARGIN as usize {
            MARGIN as usize
        } else {
            0
        };
        let mut out: Vec<Line> = Vec::with_capacity(body.len() + margin);
        for _ in 0..margin {
            out.push(Line::empty());
        }
        out.append(&mut body);
        // The host clips to the rect anyway; doing it here keeps the returned
        // lines inside the height this module was handed.
        out.truncate(vp.rect.h as usize);
        out
    }

    /// The margin and the bars, and nothing between turns.
    ///
    /// Counted by rendering at `width`, not by counting newlines: a bar wraps,
    /// so how many rows it takes is a fact about the width it is given.
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let body = if width == 0 {
            0
        } else {
            bars(moment, width).len()
        };
        if body == 0 {
            return Height::Hug(0);
        }
        Height::Hug((body + MARGIN as usize).min(u16::MAX as usize) as u16)
    }
}

/// A header, then one compact `↳ <text>` preview row per waiting message, oldest
/// first. Empty (no header) when nothing is queued.
fn bars(moment: &Moment, width: u16) -> Vec<Line> {
    let queued: Vec<&str> = moment
        .steering
        .lines()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .collect();
    if queued.is_empty() {
        return Vec::new();
    }
    // A queue, not a stack of full user bars: a header that says these are waiting
    // (folded at the next tool call, or flushed now with Esc), then one compact
    // `↳ <text>` per message oldest-first — so several lines typed ahead read as one
    // pending batch instead of N look-alike sent messages.
    let mut out: Vec<Line> = Vec::new();
    out.extend(El::styled(t(Msg::SteeringQueued), theme::fg(Role::Muted)).lay(width));
    for message in queued {
        out.extend(El::styled(format!("  ↳ {message}"), theme::fg(Role::Muted)).lay(width));
    }
    out
}

/// The blank row above the bars — the padding that keeps them off the live line
/// they sit under.
///
/// Above only, and asked for here for the reason the live line and the task list
/// give: a `gap` in the layout is counted between its children whether or not
/// this row is mounted, so between turns the composer would stand on a blank
/// row. Asked for here, it arrives and leaves with the words.
const MARGIN: u16 = 1;

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
    fn the_queue_is_a_header_then_a_row_per_message() {
        // Not a stack of transcript look-alikes: a header that says the lines are
        // waiting, then one `↳ <text>` per message.
        let m = waiting("看看 crates/ 的结构");
        let lines = draw(&m, 80, 8);
        assert_eq!(lines[0], "", "margin first");
        assert!(
            !lines[1].is_empty() && !lines[1].contains('↳'),
            "a header row, not an entry: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains('↳') && l.contains("看看 crates/ 的结构")),
            "the message is a ↳ entry: {lines:#?}"
        );
    }

    #[test]
    fn a_rect_that_cannot_seat_the_margin_keeps_the_content() {
        // Same bargain as the live line: the margin drops first, so the queue
        // shows rather than a blank row.
        let m = waiting("and also this");
        let lines = draw(&m, 80, 1);
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert_ne!(lines[0], "", "not a blank margin row: {lines:#?}");
    }

    #[test]
    fn several_lines_typed_ahead_read_as_one_pending_batch() {
        // Three follow-ups → ONE header + three ↳ rows (a batch), not three bars.
        let m = waiting("first follow-up\nsecond follow-up\nthird");
        let lines = draw(&m, 80, 10);
        assert_eq!(lines[0], "");
        let entries: Vec<&String> = lines.iter().filter(|l| l.contains('↳')).collect();
        assert_eq!(entries.len(), 3, "one ↳ per message: {lines:#?}");
        assert!(entries[0].contains("first follow-up"));
        assert!(entries[2].contains("third"));
        // margin(1) + header(1) + 3 entries = 5
        assert_eq!(Steering::height(&State, &m, 80), Height::Hug(5));
    }

    #[test]
    fn a_long_message_is_one_truncated_preview_row() {
        // A queue is a glance, not the full paste: a long entry is a single
        // truncated preview row so the panel stays compact.
        let text = "a sentence long enough that it cannot possibly fit on one row of \
                    a sixty column screen without wrapping somewhere";
        let m = waiting(text);
        let width = 60u16;
        let lines = draw(&m, width, 20);
        // margin(1) + header(1) + one (truncated) entry(1)
        assert_eq!(lines.len(), 3, "one preview row per message: {lines:#?}");
        assert!(
            lines[2].contains('↳'),
            "the entry stays one row: {lines:#?}"
        );
        assert_eq!(Steering::height(&State, &m, width), Height::Hug(3));
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
