//! The prompt line: what is being typed, and what the keys do right now.
//!
//! The text itself lives in `Moment`, not here — it is not a fact until it is
//! sent, and a module that kept its own copy would be a second home for it.

use crate::i18n::{t, Msg};
use atomcode_harness::session::SessionEvent;

use crate::frame::Line;
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
}

/// What the rule says when a password is being asked, or `None` when it is the
/// ordinary composer.
///
/// While a password is being asked the field is not the composer, so the rule
/// says what the keys do now rather than where a draft came from — the draft is
/// still in the buffer, and is not what is on the line. It is the one place the
/// two ways out are written down: the prompt itself is the asking program's
/// words and says nothing about esc. Centred, because it is one instruction
/// about the whole field rather than two facts about its shoulders.
fn secret_caption(moment: &crate::moment::Moment) -> Option<String> {
    moment
        .secret
        .is_some()
        .then(|| t(Msg::InputAnswerKeys).into_owned())
}

/// Where in the history the field is being browsed from, for the left shoulder
/// of the rule — or `None` when nobody is arrowing through it, which is
/// otherwise unanswerable from the screen (browsing looked exactly like having
/// typed the same words yourself). 1-based and counted from the newest, the
/// direction a person arrows: the first press is 1, not `history.len()`.
///
/// A free function so it can be judged without a terminal.
fn history_caption(moment: &crate::moment::Moment) -> Option<String> {
    let total = moment.history.len();
    moment.history_at.map(|at| {
        let nth = total.saturating_sub(at);
        t(Msg::InputHistoryNth { nth, total }).into_owned()
    })
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
    (h as usize).saturating_sub(RULES).clamp(1, MAX_ROWS)
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

/// What the field is showing, and where the caret sits in it.
///
/// Usually the line being typed. While a password is being asked
/// (`crate::secret`), the asking program's own words followed by one mask glyph
/// per character — and the caret at the end of them, because a mask has nothing
/// in it to move a caret through. **The password never reaches here**: what
/// arrives is a count, and the draft is left where it is, untouched, to come
/// back when the prompt closes.
///
/// One function, because everything that measures this field has to measure the
/// same one: `render` draws it, `caret` puts the cursor in it, `height` asks how
/// many rows it needs, and a click resolves against it.
fn shown(moment: &crate::moment::Moment) -> (std::borrow::Cow<'_, str>, usize) {
    use std::borrow::Cow;
    match &moment.secret {
        Some(asking) => {
            let line = asking.line(&moment.caps);
            let caret = line.len();
            (Cow::Owned(line), caret)
        }
        None => (Cow::Borrowed(moment.input.as_str()), moment.caret),
    }
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

/// The byte offset a click at a screen cell lands on.
///
/// The same window `render` drew: the field scrolls, so the row under the
/// pointer is not the row of the text. Forgiving at the edges on purpose — a
/// click past the end of a line means the end of that line, and a click on the
/// rules means the nearest row. Refusing those would make the field feel like
/// it has invisible dead zones.
pub fn offset_at_cell(
    moment: &crate::moment::Moment,
    rect: crate::frame::Rect,
    x: u16,
    y: u16,
) -> Option<usize> {
    if !rect.contains(x, y) {
        return None;
    }
    // A click cannot put the caret in a mask: there is one place to type while a
    // password is being asked, and it is the end. Nothing moves, rather than the
    // caret landing somewhere the keys would ignore.
    if moment.secret.is_some() {
        return None;
    }
    let body = body_width(rect.w);
    let (rows, (caret_row, _), _) = lay(&moment.input, moment.caret, body);
    let room = typed_room(rect.h);
    let first = caret_row.saturating_sub(room.saturating_sub(1));
    let shown = room.min(rows.len().saturating_sub(first)).max(1);

    let within = (y.saturating_sub(rect.y) as usize).saturating_sub(RULE);
    let row = first + within.min(shown - 1);
    let col = (x.saturating_sub(rect.x) as usize).saturating_sub(PROMPT);
    Some(offset_at(&moment.input, row, col, rect.w))
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
    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
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
        // The upper rule carries what is true about the field right now, laid on
        // its two shoulders the way `atomcode-tuix` does it: the history position
        // (while arrowing back) on the left, and the session's name on a pill on
        // the right. Both were otherwise unanswerable from the screen — the name
        // appeared nowhere but the terminal's title bar, and browsing history
        // looked exactly like having typed the same words yourself.
        //
        // In the rule rather than on a row of its own, because it is only
        // sometimes there and a reserved row that is usually blank is a row of
        // chrome. Each shoulder degrades to the bare rule when it will not fit,
        // so a narrow terminal loses the words, not the boundary.
        let muted = theme::fg(Role::Muted);
        let top = if let Some(keys) = secret_caption(vp.moment) {
            // A password: one centred instruction about the whole field, and no
            // name pill — what is on the line is not this session's to label.
            El::text(crate::el::captioned_rule(&keys, w as usize, muted, muted))
        } else {
            let history = history_caption(vp.moment);
            let name = vp.moment.title.as_deref().filter(|s| !s.is_empty());
            if history.is_none() && name.is_none() {
                rule()
            } else {
                El::text(crate::el::flanked_rule(
                    history.as_deref(),
                    name,
                    w as usize,
                    muted,
                    muted,
                    theme::fg(Role::Border).reverse(),
                ))
            }
        };
        let mut rows: Vec<El> = vec![top];

        // The typed text wraps rather than scrolling sideways, and the field
        // grows to hold it. Past `MAX_ROWS` it scrolls instead, keeping the
        // caret in view — `caret` below windows it the same way, from the same
        // walk, so the cursor cannot land off the text.
        let prompt = format!("{} ", vp.moment.caps.g(crate::caps::Glyph::Prompt));
        let body = body_width(w);
        let (text, at) = shown(vp.moment);
        let (typed, (caret_row, _), _) = lay(&text, at, body);
        let room = typed_room(vp.rect.h);
        let first = caret_row.saturating_sub(room.saturating_sub(1));
        for (i, piece) in typed.iter().skip(first).take(room).enumerate() {
            // The prompt marks the first row *on screen*, not the first row of
            // the text. Marking the text's first row meant a field scrolled
            // past it had no prompt anywhere — ten rows of bare text between
            // two rules, which does not read as somewhere you can type.
            let lead = if i == 0 { prompt.clone() } else { "  ".into() };
            // The typed text is the terminal's own foreground — what you are
            // composing is the one thing on this screen you are actively working
            // on, so it reads at full strength, not dimmed.
            let mut row = vec![El::styled(lead, arrow), El::raw(piece.clone())];
            // The rest of something already said, dim, on the last row of what
            // is typed — pressing right takes it. Only there, because that is
            // where the caret is when a completion means anything. Never while
            // a password is being asked: what is on the line is not the draft,
            // and a completion of the draft drawn after the mask would offer to
            // finish something nobody is typing.
            if i + first == typed.len().saturating_sub(1) && vp.moment.secret.is_none() {
                if let Some(rest) = crate::text::ghost(
                    &vp.moment.input,
                    &vp.moment.history,
                    vp.moment.history_at.is_some(),
                ) {
                    row.push(El::styled(rest.to_string(), theme::fg(Role::Muted)));
                }
            }
            rows.push(El::row(row));
        }

        rows.push(rule());

        // The slash menu is not drawn here any more. It used to hang below this
        // rule and be counted in `height`, which meant opening it pushed the
        // conversation up — a discovery surface that resizes the thing beside
        // it as it appears. It is now a floating part the host stacks over the
        // layout: it rises out of the prompt and covers what is above it, and
        // nothing else on screen moves. See `Host::menu_rect`.
        El::col(rows).lay(w)
    }

    /// The rule, the typed text, and nothing else.
    ///
    /// Asked from the moment rather than from state, because the text is not
    /// this module's state — it lives in `Moment`. Asking from state alone is
    /// what pinned the composer at one row: a pasted stack trace went into the
    /// buffer whole and was sent whole, but only its first line was ever drawn.
    fn height(_state: &State, moment: &crate::moment::Moment, width: u16) -> Height {
        let body = body_width(width);
        let (text, at) = shown(moment);
        let typed = lay(&text, at, body).0.len().min(MAX_ROWS);
        Height::Hug((RULES + typed.max(1)) as u16)
    }
}

/// Where the caret should sit — from the same walk and the same window
/// `render` used, so it cannot drift off the character it belongs to.
pub fn caret(moment: &crate::moment::Moment, rect: crate::frame::Rect) -> (u16, u16) {
    let body = body_width(rect.w);
    let (text, at) = shown(moment);
    let (_, (row, col), _) = lay(&text, at, body);
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
    //! Slicing is allowed in here: every index is a byte offset a test computed
    //! from its own ASCII fixture, and the point of the assertion is usually
    //! that offset. Production code says why each slice is safe instead; this
    //! is the one place where "the test wrote the string" is the whole reason.
    #![allow(
        clippy::string_slice,
        reason = "byte offsets over the test's own fixtures"
    )]

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

    /// The upper rule carries the history position on the left and the session's
    /// name on the right, the way the reference does it.
    ///
    /// Both were unanswerable from the screen before: the name appeared nowhere
    /// but the terminal's title bar, and browsing the history looked exactly like
    /// having typed the same words again. The history is counted from the newest
    /// and 1-based, because that is the direction a person arrows — the first
    /// press is 1.
    #[test]
    fn the_upper_rule_carries_the_history_on_the_left_and_the_name_on_the_right() {
        let mut m = Moment::default();
        assert_eq!(history_caption(&m), None, "nothing arrowed, no left caption");

        // The name alone rides the right shoulder — a resumed or renamed session
        // says which one it is even before anybody arrows the history.
        m.title = Some("修解析器".into());
        let named = draw(&State::default(), &m, 40, 3);
        assert!(named[0].contains("修解析器"), "the name is on the rule:\n{named:?}");

        m.history = vec!["one".into(), "two".into(), "three".into()];
        m.history_at = Some(2); // the first press back: the newest entry
        assert_eq!(history_caption(&m).as_deref(), Some("历史 1/3"));
        m.history_at = Some(0); // the oldest
        assert_eq!(history_caption(&m).as_deref(), Some("历史 3/3"));

        // Both reach the rule, and the history sits to the left of the name.
        let out = draw(&State::default(), &m, 40, 3);
        let left = out[0].find("3/3").expect("the history position");
        let right = out[0].find("修解析器").expect("the session name");
        assert!(left < right, "history on the left, name on the right:\n{out:?}");

        // Too narrow for either shoulder: the boundary survives, the words go.
        let narrow = draw(&State::default(), &m, 12, 3);
        assert!(
            !narrow[0].contains("历史") && !narrow[0].contains("修"),
            "{narrow:?}"
        );
        assert_eq!(
            narrow[0].chars().count(),
            12,
            "still a full-width rule: {narrow:?}"
        );
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

    /// The typed text is the terminal's own foreground — full strength, not
    /// dimmed: what you are actively composing is not chrome.
    #[test]
    fn the_typed_text_is_the_default_ink() {
        let m = Moment::default().typing("hello");
        let row = &Input::render(&State::default(), &Viewport::new(Rect::sized(40, 3), &m))[1];
        // spans[0] is the prompt marker; the typed text follows it.
        let text = row
            .spans
            .iter()
            .find(|s| s.text.contains("hello"))
            .expect("the typed text is on the first body row");
        assert_eq!(
            text.style.fg, None,
            "the typed text uses the default ink: {row:?}"
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
    fn the_field_keeps_its_height_whether_or_not_a_menu_is_open() {
        // The menu is drawn over the layout, not inside this module, so opening
        // it must not change how tall the field asks to be — that request is
        // what used to push the conversation up when a slash was typed.
        let state = State::default();
        let m = Moment::default();
        assert!(matches!(Input::height(&state, &m, 40), Height::Hug(3)));
        assert!(matches!(Input::height(&state, &m, 40), Height::Hug(3)));
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
    fn the_floor_of_the_field_is_one_row_however_short_the_rect_is() {
        // The cap is pinned above (`capped, not unbounded`); the floor was not.
        // It is the half that a height at or below the two rules runs into: the
        // rules come off first, the subtraction saturates to zero, and a room of
        // zero rows is a composer with nowhere to type — worse than a cramped
        // one, because the caret has no cell to land on.
        for h in 0..=(RULES as u16 + 1) {
            assert_eq!(
                typed_room(h),
                1,
                "at height {h} the field got {} rows",
                typed_room(h)
            );
        }
        // One row past the rules is already two, so the floor is a floor and not
        // a thumb on the scale.
        assert_eq!(typed_room(RULES as u16 + 2), 2);
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
    fn a_click_puts_the_caret_where_it_landed() {
        let rect = Rect::new(0, 5, 30, 5); // two rules, three typed rows
        let m = Moment::default().typing("hello\nsecond line\nthird");
        let at = |x, y| offset_at_cell(&m, rect, x, y).expect("inside the field");

        // Row 0 of the text is one below the top rule; the prompt eats two
        // cells, so its first character is at x = 2.
        assert_eq!(at(2, 6), 0, "the very start");
        assert_eq!(&m.input[..at(5, 6)], "hel");
        assert_eq!(&m.input[..at(2, 7)], "hello\n", "the second row");
        // x=6 is column 4 inside the field (the prompt eats two), so four
        // characters of the second row.
        assert_eq!(&m.input[..at(6, 7)], "hello\nseco");

        // Forgiving at the edges: past the end of a line is the end of it, and
        // the rules are the nearest row. Dead zones inside a text field are
        // worse than a caret that lands one cell off.
        assert_eq!(at(29, 6), 5, "past the end of `hello`");
        assert_eq!(at(2, 5), 0, "the top rule");
        assert_eq!(at(2, 9), m.input.len() - "third".len(), "the bottom rule");
        assert_eq!(offset_at_cell(&m, rect, 2, 20), None, "outside is outside");
    }

    #[test]
    fn a_click_follows_the_window_when_the_field_has_scrolled() {
        // The row under the pointer is not the row of the text once the field
        // scrolls — clicking the top visible line has to mean *that* line.
        let rect = Rect::new(0, 0, 30, RULES as u16 + 2);
        let text: String = (1..=9).map(|n| format!("line {n}\n")).collect();
        let mut m = Moment::default().typing(text.clone());
        m.caret = text.len(); // at the end, so the tail is what is shown
        let hit = offset_at_cell(&m, rect, 2, 1).expect("inside");
        assert!(
            text[hit..].starts_with("line 9"),
            "clicked the top visible row and got {:?}",
            &text[hit..hit + 8.min(text.len() - hit)]
        );
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

    /// A password `sudo` asks for is asked **on the line**, the way
    /// `atomcode-tuix` asks it: the program's own words, then one mask glyph per
    /// character, in the field — not in a box of its own.
    ///
    /// And the draft is still there underneath. A person mid-sentence when a
    /// tool call hits a `sudo` gets their sentence back; a field that had thrown
    /// it away to borrow the line would be the worse bug of the two.
    #[test]
    fn a_password_is_asked_on_the_line_and_the_draft_waits_under_it() {
        let caps = Caps::default();
        let m = Moment::default()
            .typing("把这个改好")
            .asking_password("[sudo] password for lichao:", 6);
        let out = draw(&State::default(), &m, 60, 4);

        let field = &out[1];
        assert!(
            field.starts_with(&format!(
                "{} [sudo] password for lichao: ",
                caps.g(Glyph::Prompt)
            )),
            "the asking program's own words, on the line: {field:?}"
        );
        assert_eq!(
            field.matches(caps.g(Glyph::Bullet)).count(),
            6,
            "one mask glyph per character typed: {field:?}"
        );
        assert!(
            !out.iter().any(|row| row.contains("把这个改好")),
            "the draft is not on the line while the password is: {out:?}"
        );
        // …and is untouched in the buffer, to come back when the prompt closes.
        assert_eq!(m.input, "把这个改好");

        // The rule says what the keys do now — the two ways out, which the
        // program's own prompt says nothing about.
        assert!(
            out[0].contains("enter 送出") && out[0].contains("esc 不给"),
            "the way out rides the rule: {out:?}"
        );

        // The caret sits after the last mask glyph, which is the only place
        // there is to type. `PROMPT` cells for the `❯ `, one row for the rule.
        let rect = Rect::new(0, 7, 60, 4);
        let cells = width::str_width(&format!("[sudo] password for lichao: {}", "•".repeat(6)));
        assert_eq!(caret(&m, rect), ((PROMPT + cells) as u16, 8));
        // And a click cannot put it anywhere else: there is nothing in a mask
        // to put a caret into.
        assert_eq!(offset_at_cell(&m, rect, 5, 8), None);
    }

    /// The composer keeps its own words when nothing is being asked — the other
    /// half of the judgement above, so a field that showed the prompt always
    /// (or never) fails one of them.
    #[test]
    fn with_no_password_being_asked_the_line_is_the_draft() {
        let m = Moment::default().typing("把这个改好");
        let out = draw(&State::default(), &m, 60, 4);
        assert!(out[1].contains("把这个改好"), "{out:?}");
        assert!(!out[0].contains("esc 不给"), "{out:?}");
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
