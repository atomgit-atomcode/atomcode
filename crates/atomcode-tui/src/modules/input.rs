//! The prompt line: what is being typed, and what the keys do right now.
//!
//! The text itself lives in `Moment`, not here — it is not a fact until it is
//! sent, and a module that kept its own copy would be a second home for it.

use atomcode_harness::session::SessionEvent;

use crate::frame::{Color, Line, Style};
use crate::module::{Height, View};
use crate::moment::{Activity, Viewport};
use crate::width;

pub const ID: &str = "input";

#[derive(Default)]
pub struct State {
    /// Whether anything has been said yet, so the first prompt can be helpful
    /// and later ones can get out of the way.
    pub spoke: bool,
    /// The slash menu, when a command is being typed. Set by the host, which
    /// owns the command registry — the module only draws it.
    pub menu: Vec<(String, String)>,
}

/// Rows the border eats, and cells the prompt eats. Named because `render` and
/// `caret` must agree about them, and two magic numbers would eventually not.
const BORDER: usize = 2;
const PROMPT: usize = 2;

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
        use crate::el::El;

        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let hint = match vp.moment.activity {
            Activity::Working => "esc 停止 · 输入即插话",
            Activity::Stopping => "停止中…",
            Activity::Idle if !state.spoke => "回车发送 · / 看命令 · ctrl-d 退出",
            Activity::Idle => "回车发送 · ctrl-d 退出",
        };
        let arrow = Style::new()
            .fg(Color::Ansi(if vp.moment.activity == Activity::Working {
                214
            } else {
                75
            }))
            .bold();

        // The typed line wraps rather than scrolling sideways: a person editing
        // a long prompt needs to see all of it. `body` is the width inside the
        // border and the two-cell prompt, and `caret` below computes from the
        // same number — one definition, so the cursor cannot land off the text.
        let body = (w as usize).saturating_sub(BORDER + PROMPT).max(1);
        let prompt = format!("{} ", vp.moment.caps.g(crate::caps::Glyph::Prompt));
        let mut rows: Vec<El> = Vec::new();
        for (i, piece) in width::wrap(&vp.moment.input, body).into_iter().enumerate() {
            rows.push(El::row(vec![
                El::styled(if i == 0 { prompt.clone() } else { "  ".into() }, arrow),
                El::raw(piece),
            ]));
        }
        if rows.is_empty() {
            rows.push(El::row(vec![El::styled(prompt, arrow)]));
        }

        // The menu sits under the line and takes what is left. A discovery
        // surface that pushed the prompt off the screen would be worse than no
        // discovery surface.
        if !state.menu.is_empty() {
            let room = (vp.rect.h as usize).saturating_sub(rows.len() + BORDER);
            for (name, about) in state.menu.iter().take(room) {
                rows.push(El::row(vec![
                    El::styled(format!("  /{name}"), Style::new().fg(Color::Ansi(75))),
                    El::styled(format!("  {about}"), Style::new().dim()),
                ]));
            }
        }

        // The hint rides the bottom rule. On its own line it reads as something
        // said; on the border it reads as what it is — chrome.
        El::framed(El::col(rows)).footer(hint).lay(w)
    }

    fn set_menu(state: &mut State, menu: Vec<(String, String)>) {
        state.menu = menu;
    }

    fn height(state: &State) -> Height {
        // One typed row plus the border; more while the menu is open, given
        // back after.
        Height::Hug(if state.menu.is_empty() {
            1 + BORDER as u16
        } else {
            (state.menu.len() as u16 + 1 + BORDER as u16).min(14)
        })
    }
}

/// Where the caret should sit, given the same wrapping and the same border
/// `render` used.
pub fn caret(moment: &crate::moment::Moment, rect: crate::frame::Rect) -> (u16, u16) {
    let body = (rect.w as usize).saturating_sub(BORDER + PROMPT).max(1);
    let before = &moment.input[..moment.caret.min(moment.input.len())];
    let cells = width::str_width(before);
    let row = cells / body;
    let col = cells % body;
    // +1 for the left border, +2 for the prompt; +1 down for the top rule.
    (
        rect.x + 1 + PROMPT as u16 + col as u16,
        rect.y + 1 + row.min(rect.h.saturating_sub(2).max(1) as usize) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Caps;
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

    /// The whole box, joined — so a test asserts what is on screen rather than
    /// which row it landed on. Rows moved when the border arrived; what the
    /// tests were actually about did not.
    fn screen(state: &State, moment: &Moment, w: u16, h: u16) -> String {
        draw(state, moment, w, h).join("\n")
    }

    #[test]
    fn the_first_prompt_helps_and_later_ones_get_out_of_the_way() {
        let fresh = screen(&State::default(), &Moment::default(), 60, 3);
        assert!(fresh.contains("ctrl-d"), "{fresh}");
        assert!(fresh.contains("命令"), "the first one says more");

        let mut used = State::default();
        Input::absorb(
            &mut used,
            &SessionEvent::UserMessage {
                turn: 1,
                text: "hi".into(),
                images: Vec::new(),
            },
        );
        assert!(!screen(&used, &Moment::default(), 60, 3).contains("命令"));
    }

    #[test]
    fn the_hint_rides_the_bottom_rule_rather_than_taking_a_line() {
        let out = draw(&State::default(), &Moment::default(), 60, 3);
        assert_eq!(out.len(), 3, "border, one typed row, border: {out:?}");
        let g = |x| Caps::default().g(x);
        assert!(
            out[0].starts_with(g(crate::caps::Glyph::TopLeft))
                && out[0].ends_with(g(crate::caps::Glyph::TopRight))
        );
        assert!(
            out[1].contains(Caps::default().g(crate::caps::Glyph::Prompt)),
            "the prompt is inside the box: {out:?}"
        );
        assert!(
            out[2].starts_with(g(crate::caps::Glyph::BottomLeft)) && out[2].contains("ctrl-d"),
            "the hint is chrome, so it sits on the chrome: {out:?}"
        );
    }

    #[test]
    fn every_row_of_the_box_is_exactly_the_width() {
        // Misaligned borders are the most visible way a TUI looks broken, and
        // the width arithmetic that produces them is easy to get wrong once and
        // never notice — CJK in the hint, a wide glyph, an off-by-one.
        for w in [12u16, 20, 41, 60, 80] {
            let m = Moment::default().typing("宽字符 mixed with ascii");
            for line in draw(&State::default(), &m, w, 4) {
                assert_eq!(width::str_width(&line), w as usize, "w={w}: {line:?}");
            }
        }
    }

    #[test]
    fn typing_during_a_turn_says_it_will_be_folded_in() {
        let m = Moment::default().working().typing("wait");
        let out = screen(&State::default(), &m, 60, 3);
        assert!(out.contains("wait"));
        assert!(out.contains("插话"), "{out}");
    }

    #[test]
    fn a_long_line_wraps_instead_of_scrolling_out_of_sight() {
        let m = Moment::default().typing("x".repeat(50));
        let out = draw(&State::default(), &m, 20, 6);
        assert!(
            out.len() >= 5,
            "50 chars at body 16 needs 4 rows plus chrome: {out:?}"
        );
        assert!(out.iter().all(|l| width::str_width(l) <= 20));
    }

    #[test]
    fn the_caret_follows_the_same_wrapping_the_text_used() {
        let rect = Rect::new(0, 10, 20, 3);
        let m = Moment::default().typing("x".repeat(20));
        let (x, y) = caret(&m, rect);
        // width 20 − 2 border − 2 prompt = 16 body cells, so 20 typed cells
        // wrap to row 2 column 5: x = 0 + 1 border + 2 prompt + 4, y = 10 + 1
        // top rule + 1 row.
        assert_eq!((x, y), (7, 12));
    }
}
