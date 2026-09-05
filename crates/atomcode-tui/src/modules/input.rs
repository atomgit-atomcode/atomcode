//! The prompt line: what is being typed, and what the keys do right now.
//!
//! The text itself lives in `Moment`, not here — it is not a fact until it is
//! sent, and a module that kept its own copy would be a second home for it.

use atomcode_harness::session::SessionEvent;

use crate::frame::{Line, Style};
use crate::module::{Height, View};
use crate::moment::{Activity, Viewport};
use crate::theme::{self, Role};
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

/// Rows the rule above the prompt eats, and cells the prompt itself eats.
/// Named because `render` and `caret` must agree about them, and two magic
/// numbers would eventually not.
const RULE: usize = 1;
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

    /// A rule, then the prompt. No box.
    ///
    /// `atomcode-tuix` frames the composer with a full-width rule above it and
    /// nothing else — the status line below carries what a hint would have said.
    /// A four-sided box looked tidier in isolation and wrong beside the product
    /// people already use, which is the only comparison that matters.
    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        use crate::el::El;

        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let t = vp.moment.caps.theme;
        let arrow = theme::fg(
            if vp.moment.activity == Activity::Working {
                Role::Warning
            } else {
                Role::Accent
            },
            t,
        )
        .bold();

        let mut rows: Vec<El> = vec![El::text(crate::el::plain_rule(
            w as usize,
            theme::fg(Role::Muted, t),
        ))];

        // The typed line wraps rather than scrolling sideways. `body` is the
        // width inside the prompt, and `caret` computes from the same number —
        // one definition, so the cursor cannot land off the text.
        let prompt = format!("{} ", vp.moment.caps.g(crate::caps::Glyph::Prompt));
        let body = (w as usize).saturating_sub(PROMPT).max(1);
        let typed = width::wrap(&vp.moment.input, body);
        if typed.is_empty() {
            rows.push(El::row(vec![El::styled(prompt.clone(), arrow)]));
        }
        for (i, piece) in typed.into_iter().enumerate() {
            rows.push(El::row(vec![
                El::styled(if i == 0 { prompt.clone() } else { "  ".into() }, arrow),
                El::raw(piece),
            ]));
        }

        // The menu takes what is left. A discovery surface that pushed the
        // prompt off the screen would be worse than no discovery surface.
        if !state.menu.is_empty() {
            let room = (vp.rect.h as usize).saturating_sub(rows.len());
            for (name, about) in state.menu.iter().take(room) {
                rows.push(El::row(vec![
                    El::styled(format!("  /{name}"), theme::fg(Role::Accent, t)),
                    El::styled(format!("  {about}"), Style::new().dim()),
                ]));
            }
        }
        El::col(rows).lay(w)
    }

    fn set_menu(state: &mut State, menu: Vec<(String, String)>) {
        state.menu = menu;
    }

    fn height(state: &State) -> Height {
        // One typed row plus the border; more while the menu is open, given
        // back after.
        Height::Hug(if state.menu.is_empty() {
            1 + RULE as u16
        } else {
            (state.menu.len() as u16 + 1 + RULE as u16).min(14)
        })
    }
}

/// Where the caret should sit, given the same wrapping and the same border
/// `render` used.
pub fn caret(moment: &crate::moment::Moment, rect: crate::frame::Rect) -> (u16, u16) {
    let body = (rect.w as usize).saturating_sub(PROMPT).max(1);
    let before = &moment.input[..moment.caret.min(moment.input.len())];
    let cells = width::str_width(before);
    let row = cells / body;
    let col = cells % body;
    // The prompt eats two cells; the rule above eats one row.
    (
        rect.x + PROMPT as u16 + col as u16,
        rect.y + RULE as u16 + row.min(rect.h.saturating_sub(RULE as u16).max(1) as usize) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{Caps, Glyph};
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
    fn a_rule_separates_the_composer_from_the_transcript() {
        // tuix frames the composer with one full-width rule above it and
        // nothing else. A four-sided box looked tidier in isolation and wrong
        // beside the product people already use.
        let out = draw(&State::default(), &Moment::default(), 40, 3);
        let horizontal = Caps::default().g(Glyph::Horizontal);
        assert_eq!(out[0], horizontal.repeat(40), "{out:?}");
        assert!(
            out[1].starts_with(Caps::default().g(Glyph::Prompt)),
            "and the prompt sits directly under it: {out:?}"
        );
        assert!(
            !out.iter().skip(1).any(|l| l.contains(horizontal)),
            "no second rule, no box: {out:?}"
        );
    }

    #[test]
    fn a_turn_in_progress_shows_in_the_prompt_itself() {
        // The composer has no hint line any more — the status line below says
        // what is happening. What is left here is the prompt's own colour, so
        // it has to actually change.
        let idle = Moment::default();
        let vp_idle = Viewport::new(Rect::sized(40, 3), &idle);
        let working = Moment::default().working();
        let vp_busy = Viewport::new(Rect::sized(40, 3), &working);
        let colour = |vp: &Viewport<'_>| Input::render(&State::default(), vp)[1].spans[0].style.fg;
        assert_ne!(
            colour(&vp_idle),
            colour(&vp_busy),
            "a busy prompt must look different from an idle one"
        );
    }

    #[test]
    fn a_long_line_wraps_instead_of_scrolling_out_of_sight() {
        let m = Moment::default().typing("x".repeat(50));
        let out = draw(&State::default(), &m, 20, 6);
        assert!(
            out.len() >= 4,
            "50 chars at body 18 needs 3 rows plus the rule: {out:?}"
        );
        assert!(out.iter().all(|l| width::str_width(l) <= 20));
    }

    #[test]
    fn every_row_is_within_the_width() {
        // Misaligned chrome is the most visible way a TUI looks broken, and the
        // arithmetic that produces it is easy to get wrong once and never
        // notice — CJK, a wide glyph, an off-by-one.
        for w in [8u16, 12, 20, 41, 60, 80] {
            let m = Moment::default().typing("宽字符 mixed with ascii");
            for line in draw(&State::default(), &m, w, 5) {
                assert!(width::str_width(&line) <= w as usize, "w={w}: {line:?}");
            }
        }
    }

    #[test]
    fn the_slash_menu_appears_under_the_prompt_and_gives_the_room_back() {
        let mut state = State::default();
        assert!(matches!(Input::height(&state), Height::Hug(2)));
        Input::set_menu(&mut state, vec![("help".into(), "看命令".into())]);
        let out = draw(&state, &Moment::default(), 40, 6);
        assert!(out.iter().any(|l| l.contains("/help")), "{out:?}");
        assert!(matches!(Input::height(&state), Height::Hug(n) if n > 2));
    }

    #[test]
    fn the_caret_follows_the_same_wrapping_the_text_used() {
        let rect = Rect::new(0, 10, 20, 3);
        let m = Moment::default().typing("x".repeat(20));
        let (x, y) = caret(&m, rect);
        // width 20 − 2 prompt = 18 body cells, so 20 typed cells wrap to row 2
        // column 3: x = 0 + 2 prompt + 2, y = 10 + 1 rule + 1 row.
        assert_eq!((x, y), (4, 12));
    }
}
