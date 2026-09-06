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

/// Rows the rules above and below the field eat, and cells the prompt eats.
/// Named because `render`, `height` and `caret` must all agree about them, and
/// three magic numbers would eventually not.
const RULE: usize = 1;
const RULES: usize = 2;
const PROMPT: usize = 2;

/// The most rows the composer will take, however much is put into it.
///
/// It grows with what is typed or pasted, and stops here — a thirty-line paste
/// must not swallow the conversation it is about. Past this it scrolls, keeping
/// the caret in view.
const MAX_ROWS: usize = 10;

/// How many rows of typed text fit in a rect this tall.
fn typed_room(h: u16) -> usize {
    (h as usize).saturating_sub(RULES).min(MAX_ROWS).max(1)
}

/// The typed text as it is drawn, and where the caret lands in it.
///
/// **One walk, so the two answers cannot disagree.** They used to share only an
/// arithmetic identity: `render` called `width::wrap`, and `caret` computed
/// `cells / body`. That was already wrong whenever a word wrapped early, and
/// wrong for every row the moment a newline could reach the buffer — which is
/// exactly what bracketed paste made possible.
///
/// Hard wrapping, deliberately, and not [`width::wrap`]: the caret has to sit
/// on the character it is next to, and a word that jumps to the next row moves
/// every column after it. Word wrapping is right for prose in the transcript
/// and wrong for a field being edited.
fn lay(input: &str, caret: usize, body: usize) -> (Vec<String>, (usize, usize), Vec<usize>) {
    use unicode_segmentation::UnicodeSegmentation;

    let body = body.max(1);
    let caret = caret.min(input.len());
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut used = 0usize;
    let mut at = (0usize, 0usize);
    let mut offset = 0usize;
    // Where each drawn row begins in the buffer. Moving the caret up a row is
    // the inverse of this walk, and doing it in a second walk is how `render`
    // and `caret` came to disagree in the first place.
    let mut starts = vec![0usize];

    for (n, logical) in input.split('\n').enumerate() {
        if n > 0 {
            rows.push(std::mem::take(&mut row));
            used = 0;
            offset += 1; // the newline itself
            starts.push(offset);
            if offset <= caret {
                at = (rows.len(), 0);
            }
        }
        for g in logical.graphemes(true) {
            let gw = width::str_width(g);
            // Wider than the whole field: it cannot be shown at this width by
            // any means. It stays in the buffer and the caret still counts it.
            if gw > body {
                offset += g.len();
                if offset <= caret {
                    at = (rows.len(), used);
                }
                continue;
            }
            if used + gw > body {
                rows.push(std::mem::take(&mut row));
                used = 0;
                starts.push(offset);
            }
            row.push_str(g);
            used += gw;
            offset += g.len();
            if offset <= caret {
                at = (rows.len(), used);
            }
        }
    }
    rows.push(row);

    // A caret sitting just past the last cell of a full row belongs at the
    // start of the next one — otherwise it is drawn one column outside the
    // field, which on the last row is one column outside the screen.
    if at.1 >= body {
        at = (at.0 + 1, 0);
        if at.0 >= rows.len() {
            rows.push(String::new());
            starts.push(input.len());
        }
    }
    (rows, at, starts)
}

/// Where the caret sits in the drawn text: its row, its column, and how many
/// rows there are.
///
/// What the host needs to decide whether an up-arrow is "one row up" or "the
/// previous thing I said": inside the text it moves, at the edge it hands over.
pub fn caret_row(input: &str, caret: usize, width: u16) -> (usize, usize, usize) {
    let (rows, (row, col), _) = lay(input, caret, body_width(width));
    (row, col, rows.len())
}

/// The byte offset the caret lands on when it moves to `(row, col)`.
///
/// The inverse of the same walk that drew the rows, so moving up and then down
/// again returns to where it started — as long as nothing was typed in between,
/// which is exactly the guarantee a person expects from an arrow key.
pub fn offset_at(input: &str, row: usize, col: usize, width: u16) -> usize {
    let body = body_width(width);
    let (rows, _, starts) = lay(input, 0, body);
    let row = row.min(rows.len().saturating_sub(1));
    let start = starts.get(row).copied().unwrap_or(0).min(input.len());
    let prefix = width::take_width(&rows[row], col);
    (start + prefix.len()).min(input.len())
}

/// The cells inside the prompt. One definition, because `render`, `caret` and
/// every caller that moves by rows must measure the same field.
pub fn body_width(width: u16) -> usize {
    (width as usize).saturating_sub(PROMPT).max(1)
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

    /// A rule, the prompt, a rule. Two lines, not four.
    ///
    /// The composer is *enclosed* rather than merely separated from the
    /// transcript above it: with one rule the field ran into the status line
    /// and there was nothing to say where typing stopped. Two full-width rules
    /// draw the box without drawing a box — no corners, no verticals, so
    /// nothing has to line up and the field can grow a row without the frame
    /// needing to know.
    ///
    /// (This departs from `atomcode-tuix`, which uses one rule. The two front
    /// ends now differ here on purpose.)
    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        use crate::el::El;

        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let arrow = theme::fg(if vp.moment.activity == Activity::Working {
            Role::Warning
        } else {
            Role::Accent
        })
        .bold();

        let rule = || El::text(crate::el::plain_rule(w as usize, theme::fg(Role::Muted)));
        let mut rows: Vec<El> = vec![rule()];

        // The typed text wraps rather than scrolling sideways, and the field
        // grows to hold it. Past `MAX_ROWS` it scrolls instead, keeping the
        // caret in view — `caret` below windows it the same way, from the same
        // walk, so the cursor cannot land off the text.
        let prompt = format!("{} ", vp.moment.caps.g(crate::caps::Glyph::Prompt));
        let body = body_width(w);
        let (typed, (caret_row, _), _) = lay(&vp.moment.input, vp.moment.caret, body);
        let room = typed_room(vp.rect.h);
        let first = caret_row.saturating_sub(room.saturating_sub(1));
        for (i, piece) in typed.iter().skip(first).take(room).enumerate() {
            // The prompt marks the first row *on screen*, not the first row of
            // the text. Marking the text's first row meant a field scrolled
            // past it had no prompt anywhere — ten rows of bare text between
            // two rules, which does not read as somewhere you can type.
            let lead = if i == 0 { prompt.clone() } else { "  ".into() };
            rows.push(El::row(vec![
                El::styled(lead, arrow),
                El::raw(piece.clone()),
            ]));
        }

        rows.push(rule());

        // The menu hangs below the closed field rather than inside it — it is
        // not something being typed. A discovery surface that pushed the prompt
        // off the screen would be worse than no discovery surface.
        if !state.menu.is_empty() {
            let room = (vp.rect.h as usize).saturating_sub(rows.len());
            for (name, about) in state.menu.iter().take(room) {
                rows.push(El::row(vec![
                    El::styled(format!("  /{name}"), theme::fg(Role::Accent)),
                    El::styled(format!("  {about}"), Style::new().dim()),
                ]));
            }
        }
        El::col(rows).lay(w)
    }

    fn set_menu(state: &mut State, menu: Vec<(String, String)>) {
        state.menu = menu;
    }

    /// The rule, the typed text, and the menu when it is open.
    ///
    /// Asked from the moment rather than from state, because the text is not
    /// this module's state — it lives in `Moment`. Asking from state alone is
    /// what pinned the composer at one row: a pasted stack trace went into the
    /// buffer whole and was sent whole, but only its first line was ever drawn.
    fn height(state: &State, moment: &crate::moment::Moment, width: u16) -> Height {
        let body = body_width(width);
        let typed = lay(&moment.input, moment.caret, body).0.len().min(MAX_ROWS);
        let rows = RULES + typed.max(1);
        Height::Hug(if state.menu.is_empty() {
            rows as u16
        } else {
            (rows + state.menu.len()).min(14) as u16
        })
    }
}

/// Where the caret should sit — from the same walk and the same window
/// `render` used, so it cannot drift off the character it belongs to.
pub fn caret(moment: &crate::moment::Moment, rect: crate::frame::Rect) -> (u16, u16) {
    let body = body_width(rect.w);
    let (_, (row, col), _) = lay(&moment.input, moment.caret, body);
    let room = typed_room(rect.h);
    let first = row.saturating_sub(room.saturating_sub(1));
    // The prompt eats two cells; the rule above eats one row.
    (
        rect.x + PROMPT as u16 + col as u16,
        rect.y + RULE as u16 + (row - first) as u16,
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
    fn two_rules_enclose_the_composer_without_drawing_a_box() {
        // A rule above *and* below. With only the one above, the field ran into
        // the status line and nothing said where typing stopped. Two full-width
        // rules close it without corners or verticals — so nothing has to line
        // up, and the field can grow a row without the frame knowing.
        let out = draw(&State::default(), &Moment::default(), 40, 3);
        let caps = Caps::default();
        let rule = caps.g(Glyph::Horizontal).repeat(40);
        assert_eq!(out.first(), Some(&rule), "{out:?}");
        assert_eq!(out.last(), Some(&rule), "{out:?}");
        assert!(
            out[1].starts_with(caps.g(Glyph::Prompt)),
            "the prompt sits between them: {out:?}"
        );
        for corner in [Glyph::TopLeft, Glyph::BottomRight, Glyph::Vertical] {
            assert!(
                !out.iter().any(|l| l.contains(caps.g(corner))),
                "{corner:?} — two lines, not four: {out:?}"
            );
        }
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
        let m = Moment::default();
        assert!(matches!(Input::height(&state, &m, 40), Height::Hug(3)));
        Input::set_menu(&mut state, vec![("help".into(), "看命令".into())]);
        let out = draw(&state, &Moment::default(), 40, 6);
        let at = out
            .iter()
            .position(|l| l.contains("/help"))
            .expect("no menu");
        assert!(
            at > out
                .iter()
                .rposition(|l| l.starts_with(Caps::default().g(Glyph::Horizontal)))
                .unwrap(),
            "the menu hangs below the closed field, not inside it: {out:?}"
        );
        assert!(matches!(Input::height(&state, &m, 40), Height::Hug(n) if n > 3));
    }

    #[test]
    fn the_composer_grows_with_what_is_put_into_it() {
        // The gap bracketed paste opened: the text went into the buffer whole
        // and was sent whole, but the field asked for one row, so only the
        // first line of a pasted stack trace was ever drawn.
        let state = State::default();
        let one = Moment::default().typing("one line");
        assert!(matches!(Input::height(&state, &one, 40), Height::Hug(3)));

        let pasted = Moment::default().typing("line one\nline two\nline three");
        let Height::Hug(n) = Input::height(&state, &pasted, 40) else {
            panic!("the composer hugs its content");
        };
        assert_eq!(n as usize, RULES + 3, "the two rules plus three typed rows");

        let out = draw(&state, &pasted, 40, n);
        assert!(out[1].contains("line one"), "{out:?}");
        assert!(out[3].contains("line three"), "{out:?}");
    }

    #[test]
    fn a_paste_longer_than_the_cap_scrolls_instead_of_eating_the_screen() {
        let state = State::default();
        let long = (1..=30)
            .map(|n| format!("row {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let m = Moment::default().typing(long.clone());
        let Height::Hug(n) = Input::height(&state, &m, 40) else {
            panic!("hug");
        };
        assert_eq!(n as usize, RULES + MAX_ROWS, "capped, not unbounded");

        // The caret is at the end, so the tail is what is shown.
        let mut at_end = m.clone();
        at_end.caret = long.len();
        let out = draw(&state, &at_end, 40, n);
        assert!(out[out.len() - 2].contains("row 30"), "{out:?}");
        assert!(!out.iter().any(|l| l.contains("row 1 ")), "{out:?}");

        // And with the caret at the top, the head is.
        let mut at_start = m.clone();
        at_start.caret = 0;
        let out = draw(&state, &at_start, 40, n);
        assert!(out[1].contains("row 1"), "{out:?}");
    }

    #[test]
    fn the_caret_lands_on_the_character_it_is_next_to_at_any_offset() {
        // The property the shared walk exists for, checked over every caret
        // position rather than the two that were tried by hand. `render` and
        // `caret` disagreeing is a cursor sitting on the wrong character —
        // which is what `cells / body` did the moment a newline could appear.
        let rect = Rect::new(0, 5, 24, 8);
        let body = body_width(24);
        for text in [
            "short",
            "a much longer line that has to wrap more than once at this width",
            "one\ntwo\nthree",
            "宽字符 mixed 中文 and ascii\nsecond line",
            "trailing newline\n",
            "",
        ] {
            for caret in 0..=text.len() {
                if !text.is_char_boundary(caret) {
                    continue;
                }
                let mut m = Moment::default().typing(text);
                m.caret = caret;
                let (rows, (row, col), _) = lay(text, caret, body);
                let (x, y) = super::caret(&m, rect);
                assert!(
                    col < body,
                    "{text:?}@{caret}: column {col} is outside the field"
                );
                assert!(
                    row < rows.len(),
                    "{text:?}@{caret}: row {row} of {}",
                    rows.len()
                );
                assert!(
                    x >= rect.x + PROMPT as u16 && x < rect.x + rect.w,
                    "{text:?}@{caret}: x={x} outside {rect:?}"
                );
                assert!(
                    y >= rect.y + RULE as u16 && y < rect.y + rect.h,
                    "{text:?}@{caret}: y={y} outside {rect:?}"
                );
            }
        }
    }

    #[test]
    fn moving_by_rows_is_the_exact_inverse_of_drawing_them() {
        // Down then up has to land where it started, or an arrow key is a
        // guess. Both directions go through the same walk that drew the rows,
        // which is the only reason this holds for wrapped and CJK text too.
        for text in [
            "one\ntwo\nthree",
            "a much longer line that wraps more than once at this width",
            "宽字符 中文 mixed\nsecond line here",
        ] {
            for caret in (0..=text.len()).filter(|c| text.is_char_boundary(*c)) {
                let (row, col, rows) = caret_row(text, caret, 24);
                if row + 1 >= rows {
                    continue;
                }
                let down = offset_at(text, row + 1, col, 24);
                let (r2, _, _) = caret_row(text, down, 24);
                assert_eq!(r2, row + 1, "{text:?}@{caret} did not move down a row");
                let back = offset_at(text, row, col, 24);
                assert_eq!(back, caret, "{text:?}@{caret}: down then up moved it");
            }
        }
    }

    #[test]
    fn the_edges_of_the_text_are_where_the_arrows_hand_over() {
        // The whole rule in one place: inside the text the arrows move, at its
        // edge they belong to the history instead.
        let one = "single line";
        let (row, _, rows) = caret_row(one, 3, 40);
        assert_eq!((row, rows), (0, 1), "one row is both edges at once");

        let three = "one\ntwo\nthree";
        assert_eq!(caret_row(three, 0, 40).0, 0, "top");
        assert_eq!(caret_row(three, 5, 40).0, 1, "middle: neither edge");
        let (row, _, rows) = caret_row(three, three.len(), 40);
        assert_eq!(row + 1, rows, "bottom");
    }

    #[test]
    fn wrapping_is_hard_so_a_column_means_the_same_thing_in_both_answers() {
        // Word wrapping is right for prose and wrong for a field being edited:
        // a word jumping to the next row moves every column after it, and the
        // caret would follow the arithmetic rather than the character.
        let (rows, _, _) = lay("aaa bbb ccc", 0, 4);
        assert_eq!(rows, vec!["aaa ", "bbb ", "ccc"], "broken at the edge");
        // Two cells each, so a three-cell field holds one per row — a wide
        // character is never halved to fill the gap.
        let (rows, at, _) = lay("中文中文", 0, 3);
        assert_eq!(rows, vec!["中", "文", "中", "文"], "never mid-character");
        assert_eq!(at, (0, 0));
    }

    #[test]
    fn the_caret_follows_the_same_wrapping_the_text_used() {
        // Four rows: two rules and two of typed text, which is what 20 cells
        // need at this width.
        let rect = Rect::new(0, 10, 20, 4);
        let m = Moment::default().typing("x".repeat(20));
        let (x, y) = caret(&m, rect);
        // width 20 − 2 prompt = 18 body cells, so 20 typed cells wrap to row 2
        // column 3: x = 0 + 2 prompt + 2, y = 10 + 1 rule + 1 row.
        assert_eq!((x, y), (4, 12));

        // And in a field too short for both, the caret's row is the one shown.
        let (_, y) = caret(&m, Rect::new(0, 10, 20, 3));
        assert_eq!(y, 11, "scrolled to the caret, still inside the rules");
    }
}
