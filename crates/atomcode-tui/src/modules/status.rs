//! The one-line header: what is running, on what, at what cost.

use atomcode_harness::session::SessionEvent;

// One frame per tick, shared with the live line above the composer: two panels
// cycling two different sets, or the same set out of phase, is two front ends
// on one screen. Which set is the terminal's answer, so it comes from `caps`.
use crate::frame::{Color, Line, Style};
use crate::module::{Height, View};
use crate::moment::{Activity, Moment, Viewport};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "status";

#[derive(Default)]
pub struct State {
    pub turns: u64,
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub tool_calls: u32,
    pub last_stop: Option<String>,
}

pub struct Status;

impl View for Status {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(state: &mut State, fact: &SessionEvent) {
        match fact {
            SessionEvent::TurnStart { turn } => {
                state.turns = *turn;
                state.last_stop = None;
            }
            SessionEvent::RequestHeader { model, .. } => state.model = model.clone(),
            SessionEvent::Usage { usage, .. } => {
                // Field-wise max, not sum: providers re-send a growing
                // cumulative figure, and summing would double-count it.
                state.prompt_tokens = state.prompt_tokens.max(usage.prompt);
                state.completion_tokens += usage.completion;
            }
            SessionEvent::StepEnd { tool_calls, .. } => state.tool_calls += tool_calls,
            SessionEvent::TurnEnd { stop, .. } => state.last_stop = Some(format!("{stop:?}")),
            _ => {}
        }
    }

    /// The bottom line: what model, where, how much context.
    ///
    /// `atomcode-tuix` puts this last and dims it — it is the thing you glance
    /// at, not the thing you read. A reverse-video bar across the top is what
    /// an editor does; a coding agent's screen belongs to the conversation.
    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        use crate::caps::Glyph;
        use crate::el::El;

        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let caps = vp.moment.caps;
        let dim = theme::fg(Role::Muted);
        let sep = || El::styled(format!(" {} ", caps.g(Glyph::Separator)), dim);

        let mut row: Vec<El> = Vec::new();
        row.push(El::styled(
            if state.model.is_empty() {
                "atomcode".to_string()
            } else {
                state.model.clone()
            },
            dim,
        ));
        if !vp.moment.cwd.is_empty() {
            row.push(sep());
            row.push(El::styled(vp.moment.cwd.clone(), dim));
        }
        // Only what means something. A counter reading zero is noise pretending
        // to be information.
        if state.prompt_tokens > 0 {
            row.push(sep());
            row.push(El::styled(
                format!("{} tok", crate::content::token_count(state.prompt_tokens)),
                dim,
            ));
        }
        match vp.moment.activity {
            // The phase comes from the injected tick, never a clock
            // (docs/adr/0008).
            Activity::Working => {
                row.push(sep());
                row.push(El::styled(
                    format!("{} 运行中", vp.moment.caps.spinner(vp.moment.tick)),
                    theme::fg(Role::Warning),
                ));
            }
            Activity::Stopping => {
                row.push(sep());
                row.push(El::styled("停止中", theme::fg(Role::Error)));
            }
            Activity::Idle => {}
        }
        El::row(row).lay(w)
    }

    fn height(_: &State, _: &Moment, _: u16) -> Height {
        Height::Fixed(1)
    }

    /// The spinner needs frames; nothing else here does. Asking always is fine
    /// because an idle screen renders identically each time — the host repaints
    /// what changed, and nothing changed.
    fn tick() -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(110))
    }
}

/// A cat that reacts to what the agent is doing.
///
/// Its state is a *mood* folded from facts; its *phase* comes from
/// `moment.tick`. `render` stays pure — animation is an input, not a clock
/// read. See `docs/adr/0008`.
pub struct Mascot;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mood {
    #[default]
    Idle,
    Thinking,
    Happy,
    Sad,
}

/// Braille dot bit, and the sub-pixel grid position it stands for.
///
/// Braille packs **2×4 sub-pixels per cell**, so the art below is written as a
/// `2w × 4h` grid and folded down. That is eight times the resolution of the old
/// one-line cat, which is what makes ears and eyes possible at all. See
/// `docs/adr/0023`.
const DOTS: [(u32, usize, usize); 8] = [
    (0x01, 0, 0),
    (0x02, 0, 1),
    (0x04, 0, 2),
    (0x40, 0, 3),
    (0x08, 1, 0),
    (0x10, 1, 1),
    (0x20, 1, 2),
    (0x80, 1, 3),
];

/// How many cells wide the art is. Sixteen sub-pixel columns.
const MASCOT_CELLS: usize = 8;
/// How many rows the art occupies. Twelve sub-pixel rows ÷ four.
pub const MASCOT_ROWS: usize = 3;

/// The head, ears and chin: identical in every mood, so a mood change moves only
/// the eyes and the mouth and the cat does not appear to jump.
///
/// Read as a 16 × 12 grid; `#` is fur, space is background.
const MASCOT_EARS: [&str; 3] = ["   ##      ##   ", "  ####    ####  ", " ######  ###### "];

/// Eyes and mouth, as the two grid rows each occupies.
///
/// The eyes are **holes in the fur** rather than drawn dots, which is what keeps
/// the cat readable in one colour: a role-coloured pupil on a role-coloured head
/// would be invisible.
const EYES_OPEN: [&str; 2] = [" ###  ####  ### ", " ###  ####  ### "];
const EYES_SHUT: [&str; 2] = [" ############## ", " ############## "];
const MOUTH_SMALL: [&str; 2] = [" ######  ###### ", " ############## "];
const MOUTH_WIDE: [&str; 2] = [" ############## ", " #####    ##### "];

/// One face: ears, eyes, one filled row, mouth, chin.
fn face(eyes: [&'static str; 2], mouth: [&'static str; 2]) -> [&'static str; 12] {
    [
        MASCOT_EARS[0],
        MASCOT_EARS[1],
        MASCOT_EARS[2],
        " ############## ",
        " ############## ",
        eyes[0],
        eyes[1],
        " ############## ",
        mouth[0],
        mouth[1],
        "  ############  ",
        "    ########    ",
    ]
}

/// Idle: awake and still. One frame, so an idle screen does not repaint — the
/// reason `tick` is conditional below.
fn idle_face() -> [&'static str; 12] {
    face(EYES_OPEN, MOUTH_SMALL)
}

/// Thinking: blinking.
///
/// **The blink starts on frame 1**, not frame 0: frame 0 is the same open-eyed
/// face as Idle, so that starting a turn does not make the cat jump — but then the
/// very next frame has to differ, because "it really moves" is judged between
/// consecutive ticks. A sequence that spent two frames open was a cat that paused
/// before blinking.
fn thinking_faces() -> Vec<[&'static str; 12]> {
    vec![
        face(EYES_OPEN, MOUTH_SMALL),
        face(EYES_SHUT, MOUTH_SMALL),
        face(EYES_OPEN, MOUTH_SMALL),
        face(EYES_OPEN, MOUTH_SMALL),
        face(EYES_SHUT, MOUTH_SMALL),
        face(EYES_OPEN, MOUTH_SMALL),
        face(EYES_OPEN, MOUTH_SMALL),
        face(EYES_SHUT, MOUTH_SMALL),
    ]
}

/// Happy: a wide open mouth, drawn as a hole.
fn happy_face() -> [&'static str; 12] {
    face(EYES_OPEN, MOUTH_WIDE)
}

/// Sad: eyes lowered a sub-pixel row and the mouth gone flat.
fn sad_face() -> [&'static str; 12] {
    face([" ############## ", " ###  ####  ### "], MOUTH_SMALL)
}

/// What a terminal with no Unicode gets instead.
///
/// Braille has no ASCII stand-in — `caps.rs` explains why for the spinner ("there
/// is no one-cell ASCII stand-in that reads as motion"), and a grid of tofu is not
/// a picture, so the art is withheld and this is what remains. Plain ASCII: on
/// such a terminal the old faces were tofu anyway.
fn ascii_faces(mood: &Mood) -> &'static [&'static str] {
    match mood {
        Mood::Idle => &["(=^.^=)"],
        Mood::Thinking => &["(=^.^=)", "(=^-^=)"],
        Mood::Happy => &["(=^o^=)"],
        Mood::Sad => &["(=^;^=)"],
    }
}

/// Fold a `16 × 12` fur grid down to `8 × 3` braille cells.
fn braille(art: &[&str; 12]) -> Vec<String> {
    let grid: Vec<Vec<char>> = art.iter().map(|row| row.chars().collect()).collect();
    (0..MASCOT_ROWS)
        .map(|cy| {
            (0..MASCOT_CELLS)
                .map(|cx| {
                    let mut bits = 0u32;
                    for (bit, dx, dy) in DOTS {
                        if grid[cy * 4 + dy][cx * 2 + dx] != ' ' {
                            bits |= bit;
                        }
                    }
                    char::from_u32(0x2800 + bits).expect("a braille code point")
                })
                .collect()
        })
        .collect()
}

impl View for Mascot {
    type State = Mood;

    fn id() -> &'static str {
        "mascot"
    }

    fn absorb(mood: &mut Mood, fact: &SessionEvent) {
        *mood = match fact {
            SessionEvent::TurnStart { .. } => Mood::Thinking,
            SessionEvent::ToolResultLogged { is_error: true, .. } => Mood::Sad,
            SessionEvent::TurnEnd { error: Some(_), .. } => Mood::Sad,
            SessionEvent::TurnEnd { .. } => Mood::Happy,
            _ => *mood,
        };
    }

    fn render(mood: &Mood, vp: &Viewport<'_>) -> Vec<Line> {
        let style = Style::new().fg(Color::role(Role::Error));
        let w = vp.rect.w as usize;
        let tick = vp.moment.tick as usize;

        if !vp.moment.caps.unicode {
            let faces = ascii_faces(mood);
            let f = faces[tick % faces.len()];
            return vec![Line::styled(width::take_width(f, w), style)];
        }

        // Idle, Happy and Sad each have one frame, so `tick` changes nothing and
        // a screen sitting in one of them does not repaint. Only Thinking spends
        // frames — animation on an idle screen is bandwidth on an ssh link for
        // nothing, which is the trade this line makes.
        let thinking = thinking_faces();
        let frames: Vec<[&'static str; 12]> = match mood {
            Mood::Idle => vec![idle_face()],
            Mood::Thinking => thinking,
            Mood::Happy => vec![happy_face()],
            Mood::Sad => vec![sad_face()],
        };
        let f = &frames[tick % frames.len()];
        braille(f)
            .into_iter()
            .map(|row| Line::styled(width::take_width(&row, w), style))
            .collect()
    }

    fn height(_: &Mood, _: &Moment, _: u16) -> Height {
        // Three cells: twelve sub-pixel rows over four.
        Height::Fixed(MASCOT_ROWS as u16)
    }

    fn tick() -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(180))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::moment::Moment;

    crate::tui_conformance!(view Status as status_conformance);
    crate::tui_conformance!(view Mascot as mascot_conformance);

    fn draw<V: View>(state: &V::State, w: u16, moment: &Moment) -> String {
        let vp = Viewport::new(Rect::sized(w, 3), moment);
        V::render(state, &vp)
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_line_says_what_model_and_where() {
        let mut st = State::default();
        Status::absorb(
            &mut st,
            &SessionEvent::RequestHeader {
                turn: 1,
                round: 1,
                model: "mimo-v2.5".into(),
                reason: atomcode_harness::session::HeaderReason::Append,
            },
        );
        let m = Moment {
            cwd: "~/project/atomcode".into(),
            ..Default::default()
        };
        let line = Status::render(&st, &Viewport::new(Rect::sized(70, 1), &m))[0].plain();
        assert!(line.contains("mimo-v2.5"), "{line:?}");
        assert!(line.contains("~/project/atomcode"), "{line:?}");
    }

    #[test]
    fn a_counter_reading_zero_is_left_out() {
        // A zero is noise pretending to be information.
        let line = Status::render(
            &State::default(),
            &Viewport::new(Rect::sized(70, 1), &Moment::default()),
        )[0]
        .plain();
        assert!(!line.contains("tok"), "{line:?}");
    }

    /// A small count is a count, not a rounding of itself to nothing. `14`
    /// tokens is what a one-line exchange costs, and the line used to render it
    /// as `0k tok` — a zero it had just decided not to print.
    #[test]
    fn a_small_count_is_not_rounded_away_to_zero() {
        let st = State {
            prompt_tokens: 14,
            ..Default::default()
        };
        let line =
            Status::render(&st, &Viewport::new(Rect::sized(70, 1), &Moment::default()))[0].plain();
        assert!(line.contains("14 tok"), "{line:?}");

        // And a large one is thousands, the same way the end of a turn says it.
        let st = State {
            prompt_tokens: 123_456,
            ..Default::default()
        };
        let line =
            Status::render(&st, &Viewport::new(Rect::sized(70, 1), &Moment::default()))[0].plain();
        assert!(line.contains("123.5k tok"), "{line:?}");
    }

    #[test]
    fn it_is_a_dim_line_rather_than_a_bar_across_the_screen() {
        // tuix dims this and lets it end where its text ends. A reverse-video
        // bar filling the width is what an editor does; a coding agent's screen
        // belongs to the conversation.
        let m = Moment {
            cwd: "/tmp".into(),
            ..Default::default()
        };
        let rendered = Status::render(&State::default(), &Viewport::new(Rect::sized(70, 1), &m));
        assert!(
            rendered[0].width() < 70,
            "it must not fill the width: {:?}",
            rendered[0].plain()
        );
        assert!(
            rendered[0].spans.iter().all(|s| s.style.bg.is_none()),
            "and it must not paint a background"
        );
    }

    #[test]
    fn a_turn_in_progress_shows_here_now_that_the_composer_has_no_hint() {
        let busy = Moment::default().working();
        let line =
            Status::render(&State::default(), &Viewport::new(Rect::sized(70, 1), &busy))[0].plain();
        assert!(line.contains("运行中"), "{line:?}");
    }

    #[test]
    fn nothing_it_draws_is_wider_than_the_screen() {
        let m = Moment {
            cwd: "/a/very/long/path/that/keeps/going/and/going/and/going".into(),
            ..Default::default()
        }
        .working();
        let st = State {
            model: "some-extremely-long-model-name-v2.5-preview".into(),
            prompt_tokens: 123_456,
            ..Default::default()
        };
        for w in 1u16..80 {
            for line in Status::render(&st, &Viewport::new(Rect::sized(w, 1), &m)) {
                assert!(line.width() <= w as usize, "w={w}: {:?}", line.plain());
            }
        }
    }

    #[test]
    fn the_mascot_animates_while_thinking_and_holds_still_when_idle() {
        let at = |mood, tick| draw::<Mascot>(&mood, 20, &Moment::default().at_tick(tick));
        assert_ne!(
            at(Mood::Thinking, 0),
            at(Mood::Thinking, 1),
            "it really moves"
        );
        assert_eq!(
            at(Mood::Idle, 0),
            at(Mood::Idle, 5),
            "an idle screen must not repaint — that is bandwidth on an ssh link"
        );

        // The same property on the ASCII path, which is a different set of frames:
        // a terminal that cannot draw braille still gets a cat that blinks while
        // it thinks, and one that holds still while it does not.
        let plain = |mood, tick| {
            let m = Moment {
                caps: crate::caps::Caps::plain(),
                ..Moment::default().at_tick(tick)
            };
            Mascot::render(&mood, &Viewport::new(Rect::sized(20, 3), &m))
                .iter()
                .map(Line::plain)
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_ne!(
            plain(Mood::Thinking, 0),
            plain(Mood::Thinking, 1),
            "the ASCII cat must move too"
        );
        assert_eq!(plain(Mood::Idle, 0), plain(Mood::Idle, 5));
        assert_eq!(plain(Mood::Happy, 0), plain(Mood::Happy, 5));
    }

    #[test]
    fn every_mood_and_phase_is_well_formed() {
        // 4 moods × 12 phases, all asserted. Animation is exhaustively testable
        // because the frame is a value and the time is injected.
        for mood in [Mood::Idle, Mood::Thinking, Mood::Happy, Mood::Sad] {
            for tick in 0..12u64 {
                let m = Moment::default().at_tick(tick);
                let vp = Viewport::new(Rect::sized(20, 3), &m);
                let lines = Mascot::render(&mood, &vp);
                assert_eq!(
                    lines.len(),
                    MASCOT_ROWS,
                    "{mood:?} at tick {tick}: the art is {MASCOT_ROWS} cells tall"
                );
                for line in &lines {
                    assert!(line.width() <= 20);
                    assert!(!line.plain().is_empty());
                }
                // Every row the same width, or the cat would look sheared: the
                // braille grid is as wide as its widest sub-pixel row.
                let widths: Vec<usize> = lines.iter().map(Line::width).collect();
                assert!(
                    widths.iter().all(|w| *w == widths[0]),
                    "{mood:?} at tick {tick}: rows of different widths {widths:?}"
                );
            }
        }
    }

    #[test]
    fn a_terminal_without_unicode_gets_the_plain_cat_rather_than_tofu() {
        // Braille is deliberately absent from the downgrade table — `caps.rs`
        // explains why for the spinner, and the same holds for a grid of it. So
        // the art is withheld and ASCII remains, rather than eight columns of
        // `?` boxes.
        let plain = Moment {
            caps: crate::caps::Caps::plain(),
            ..Moment::default()
        };
        let lines = Mascot::render(&Mood::Idle, &Viewport::new(Rect::sized(20, 3), &plain));
        assert_eq!(lines.len(), 1, "one ASCII line, not three braille rows");
        let text = lines[0].plain();
        assert!(
            text.is_ascii(),
            "the fallback must be plain ASCII: {text:?}"
        );
        assert!(text.contains("(="), "{text:?}");
        // And no braille anywhere in it.
        assert!(
            !text
                .chars()
                .any(|c| (0x2800..=0x28ff).contains(&(c as u32))),
            "{text:?}"
        );
    }

    #[test]
    fn the_mood_is_visible_in_the_face() {
        // A mood nobody can see is a mood that does not exist.
        //
        // Not "four moods, four pictures at one tick": Thinking's blink **starts
        // open**, exactly like Idle, and that is right — a cat already blinking
        // when it started thinking would be a cat that jumped. What is worth
        // pinning is that each mood differs from the others *somewhere*, and that
        // each of the others reaches something Idle never shows.
        fn frame(mood: Mood, tick: u64) -> String {
            let m = Moment::default().at_tick(tick);
            Mascot::render(&mood, &Viewport::new(Rect::sized(20, 3), &m))
                .iter()
                .map(Line::plain)
                .collect::<Vec<_>>()
                .join("\n")
        }
        const TICKS: std::ops::Range<u64> = 0..12;

        let moods = [Mood::Idle, Mood::Thinking, Mood::Happy, Mood::Sad];
        for (i, a) in moods.iter().enumerate() {
            for b in moods.iter().skip(i + 1) {
                assert!(
                    TICKS.clone().any(|tick| frame(*a, tick) != frame(*b, tick)),
                    "{a:?} and {b:?} are the same picture at every tick — one of \
                     them is invisible"
                );
            }
        }

        // Idle is the one that never moves: an idle screen must not repaint, that
        // is bandwidth on an ssh link. Every other mood has to reach something
        // other than Idle, or its picture is pointless.
        let idle: Vec<String> = TICKS.clone().map(|tick| frame(Mood::Idle, tick)).collect();
        assert!(idle.iter().all(|f| *f == idle[0]), "idle moved: {idle:?}");
        for mood in [Mood::Thinking, Mood::Happy, Mood::Sad] {
            assert!(
                TICKS.clone().any(|tick| frame(mood, tick) != idle[0]),
                "{mood:?} never looks different from Idle"
            );
        }

        // The ears are shared: only the face moves, so a mood change does not make
        // the cat jump.
        let ears: Vec<String> = moods
            .iter()
            .map(|mood| {
                frame(*mood, 0)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert!(
            ears.iter().all(|e| *e == ears[0]),
            "the ears moved between moods: {ears:?}"
        );
    }
}
