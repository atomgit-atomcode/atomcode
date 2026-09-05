//! The prompt line: what is being typed, and what the keys do right now.
//!
//! The text itself lives in `Moment`, not here — it is not a fact until it is
//! sent, and a module that kept its own copy would be a second home for it.

use atomcode_harness::session::SessionEvent;

use crate::frame::{Color, Line, Span, Style};
use crate::module::{Height, View};
use crate::moment::{Activity, Viewport};
use crate::width;

pub const ID: &str = "input";

#[derive(Default)]
pub struct State {
    /// Whether anything has been said yet, so the first prompt can be helpful
    /// and later ones can get out of the way.
    pub spoke: bool,
}

pub struct Input;

impl View for Input {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(state: &mut State, fact: &SessionEvent) {
        if matches!(fact, SessionEvent::UserMessage { .. }) {
            state.spoke = true;
        }
    }

    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let hint = match vp.moment.activity {
            Activity::Working => "esc 停止 · 输入即插话",
            Activity::Stopping => "停止中…",
            Activity::Idle if !state.spoke => "输入后回车 · ctrl-d 退出 · / 看命令",
            Activity::Idle => "ctrl-d 退出",
        };
        let arrow = Style::new().fg(Color::Ansi(if vp.moment.activity == Activity::Working {
            214
        } else {
            39
        }));
        let mut lines = Vec::new();
        // The typed line wraps rather than scrolling sideways: a person editing
        // a long prompt needs to see all of it.
        let body = w.saturating_sub(2).max(1);
        let typed = width::wrap(&vp.moment.input, body);
        for (i, piece) in typed.iter().enumerate() {
            let lead = if i == 0 { "› " } else { "  " };
            lines.push(
                Line::from_spans(vec![Span::styled(lead, arrow), Span::raw(piece.clone())])
                    .truncate(w),
            );
        }
        if lines.is_empty() {
            lines.push(Line::from_spans(vec![Span::styled("› ", arrow)]).truncate(w));
        }
        if vp.rect.h as usize > lines.len() {
            lines.push(Line::styled(width::take_width(hint, w), Style::new().dim()));
        }
        lines
    }

    fn height(_: &State) -> Height {
        Height::Hug(3)
    }
}

/// Where the caret should sit, given the same wrapping `render` used.
pub fn caret(moment: &crate::moment::Moment, rect: crate::frame::Rect) -> (u16, u16) {
    let body = (rect.w as usize).saturating_sub(2).max(1);
    let before = &moment.input[..moment.caret.min(moment.input.len())];
    let cells = width::str_width(before);
    let row = cells / body;
    let col = cells % body;
    (
        rect.x + 2 + col as u16,
        rect.y + row.min(rect.h.saturating_sub(1) as usize) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::moment::Moment;

    crate::tui_conformance!(view Input as input_conformance);

    fn draw(state: &State, moment: &Moment, w: u16, h: u16) -> Vec<String> {
        let vp = Viewport::new(Rect::sized(w, h), moment);
        Input::render(state, &vp)
            .iter()
            .map(|l| l.plain())
            .collect()
    }

    #[test]
    fn the_first_prompt_helps_and_later_ones_get_out_of_the_way() {
        let fresh = draw(&State::default(), &Moment::default(), 60, 2);
        assert!(fresh[1].contains("ctrl-d"), "{fresh:?}");
        assert!(fresh[1].contains("命令"), "the first one says more");

        let mut used = State::default();
        Input::absorb(
            &mut used,
            &SessionEvent::UserMessage {
                turn: 1,
                text: "hi".into(),
                images: Vec::new(),
            },
        );
        let later = draw(&used, &Moment::default(), 60, 2);
        assert!(!later[1].contains("命令"), "{later:?}");
    }

    #[test]
    fn typing_during_a_turn_says_it_will_be_folded_in() {
        let m = Moment::default().working().typing("wait");
        let out = draw(&State::default(), &m, 60, 2);
        assert!(out[0].contains("wait"));
        assert!(out[1].contains("插话"), "{out:?}");
    }

    #[test]
    fn a_long_line_wraps_instead_of_scrolling_out_of_sight() {
        let m = Moment::default().typing("x".repeat(50));
        let out = draw(&State::default(), &m, 20, 5);
        assert!(out.len() >= 3, "50 chars at width 18 needs 3 rows: {out:?}");
        assert!(out.iter().all(|l| width::str_width(l) <= 20));
    }

    #[test]
    fn the_caret_follows_the_same_wrapping_the_text_used() {
        let rect = Rect::new(0, 10, 20, 3);
        let m = Moment::default().typing("x".repeat(20));
        let (x, y) = caret(&m, rect);
        assert_eq!((x, y), (4, 11), "20 cells at body 18 wraps to row 2, col 2");
    }
}
