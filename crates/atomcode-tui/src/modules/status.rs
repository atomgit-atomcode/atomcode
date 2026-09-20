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
        // Whose screen this is, when it is a team member's rather than the
        // lead's: everything below and everything typed is that member's.
        let viewing = &vp.moment.viewing;
        if !viewing.is_empty() && *viewing != vp.moment.lead {
            row.push(El::styled(
                format!("成员 {}", viewing.rsplit('/').next().unwrap_or(viewing)),
                theme::fg(Role::Accent),
            ));
            row.push(sep());
        }
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
        // A session working towards something on its own says so for as long as
        // it is. Here rather than on a row of its own: a line that is empty
        // whenever nothing is running costs a row of the conversation to say
        // nothing, and what this is for is the glance — "it is still going, it
        // is on round 4". `/autonomy` answers the same thing when asked; this
        // is the part that does not have to be asked.
        if let Some(running) = vp.moment.autonomy.as_ref() {
            row.push(sep());
            row.push(El::styled(autonomy_badge(running), theme::fg(Role::Accent)));
        }
        match vp.moment.activity {
            // The phase comes from the injected tick, never a clock
            // (docs/adr/0008).
            Activity::Working => {
                row.push(sep());
                row.push(El::styled(
                    working_indicator(vp.moment),
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

/// What the status line says about a session running on its own.
///
/// Short on purpose — it shares a row with the model, the directory and the
/// token count, and the long form is what `/autonomy` is for. The kind is
/// named rather than assumed (`goal` vs `loop`), because which one is running
/// is the first thing a person wants to know and the host is free to add a
/// third.
///
/// A paused one still says so: "registered but not running" is exactly the
/// state somebody would otherwise sit and wait through.
fn autonomy_badge(running: &atomcode_host_api::Running) -> String {
    let kind = if running.kind == "goal" {
        "目标"
    } else {
        "循环"
    };
    let rounds = match running.of {
        Some(of) => format!("{}/{of}", running.round),
        None => running.round.to_string(),
    };
    match running.paused.as_deref() {
        Some(why) => format!("{kind} 第 {rounds} 轮 · 停着:{why}"),
        None => format!("{kind} 第 {rounds} 轮"),
    }
}

/// The cat's working animation.
///
/// One table, because both the strip and the status line draw it: the status
/// line took the cat where `{spinner} 运行中` used to be, and a second copy of
/// the frames is exactly how the two would come to disagree about what the
/// product's cat does while it works.
///
/// The frames are why [`working_indicator`] has a fallback at all: neither `·`
/// nor `ω` is ASCII, and a frame is not something a downgrade table can rewrite
/// (see [`crate::caps::SPINNER`]).
pub const WORKING_FRAMES: [&str; 4] = ["(=^·^=)", "(=^-^=)", "(=^ω^=)", "(=^-^=)"];

/// The frame for this tick. The phase is the injected tick and never a clock
/// read in `render` — the same rule the strip follows (`docs/adr/0008`).
fn working_frame(tick: u64) -> &'static str {
    WORKING_FRAMES[(tick as usize) % WORKING_FRAMES.len()]
}

/// What the status line says while a turn is in flight.
///
/// The cat, not the words: it replaced `{spinner} 运行中` wholesale on this row.
/// Where the terminal said it cannot draw `·` or `ω` the words come back, and
/// that is not decoration — this row is on every screen, unlike the strip, and
/// a kaomoji arrives on a bare ssh client as tofu, which indicates nothing
/// (`Caps::unicode`).
fn working_indicator(m: &Moment) -> String {
    if m.caps.unicode {
        working_frame(m.tick).to_string()
    } else {
        format!("{} 运行中", m.caps.spinner(m.tick))
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
        let frames: &[&str] = match mood {
            // Idle has one frame, so `tick` changes nothing and an idle screen
            // does not repaint — the reason `tick()` below is conditional.
            Mood::Idle => &["(=^·^=)"],
            // The same frames the status line draws: one cat, not two.
            Mood::Thinking => &WORKING_FRAMES,
            Mood::Happy => &["(=^▽^=)"],
            Mood::Sad => &["(=；ω；=)"],
        };
        let f = frames[(vp.moment.tick as usize) % frames.len()];
        vec![Line::styled(
            width::take_width(f, vp.rect.w as usize),
            Style::new().fg(Color::role(Role::Error)),
        )]
    }

    fn height(_: &Mood, _: &Moment, _: u16) -> Height {
        Height::Fixed(1)
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
        let vp = Viewport::new(Rect::sized(w, 1), moment);
        V::render(state, &vp)
            .first()
            .map(|l| l.plain())
            .unwrap_or_default()
    }

    /// A session running on its own says so without being asked.
    ///
    /// The gap this closes: the fact was already kept (`Moment::autonomy`, fed
    /// by `HostEvent::Autonomy` every round) and **nothing drew it** — a person
    /// could only find out by typing `/autonomy`, which is the one thing you
    /// cannot do while wondering whether it is still going.
    ///
    /// Both halves are the criterion. It costs nothing when nothing is running:
    /// a badge that took a slot on every idle screen would be a permanent
    /// reminder of a thing that is not happening.
    #[test]
    fn a_session_running_on_its_own_says_so_and_costs_nothing_when_it_is_not() {
        let st = State::default();
        let mut m = Moment::default();

        let idle = draw::<Status>(&st, 80, &m);
        assert!(!idle.contains("轮"), "nothing is running: {idle:?}");

        m.autonomy = Some(atomcode_host_api::Running {
            kind: "goal".into(),
            what: "把量化器搬到 NPU".into(),
            round: 4,
            of: Some(12),
            elapsed_secs: 930,
            paused: None,
        });
        let running = draw::<Status>(&st, 80, &m);
        assert!(running.contains("目标"), "which kind: {running:?}");
        assert!(running.contains("4/12"), "and how far: {running:?}");

        // Registered but not running is the state somebody would otherwise sit
        // and wait through, so it is said.
        m.autonomy = Some(atomcode_host_api::Running {
            kind: "loop".into(),
            what: "跑测试".into(),
            round: 2,
            of: None,
            elapsed_secs: 5,
            paused: Some("等审批".into()),
        });
        let paused = draw::<Status>(&st, 80, &m);
        assert!(paused.contains("循环"), "{paused:?}");
        assert!(
            paused.contains("等审批"),
            "why it is not moving: {paused:?}"
        );
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
        assert!(line.contains(WORKING_FRAMES[0]), "{line:?}");
        assert!(
            !line.contains("运行中"),
            "the words were replaced, not added to: {line:?}"
        );
    }

    #[test]
    fn the_cat_animates_here_and_the_words_come_back_without_unicode() {
        let at = |tick: u64| {
            let m = Moment::default().working().at_tick(tick);
            Status::render(&State::default(), &Viewport::new(Rect::sized(70, 1), &m))[0].plain()
        };
        assert_ne!(at(0), at(1), "it really moves on this row too");
        assert_eq!(
            at(0),
            at(WORKING_FRAMES.len() as u64),
            "and it loops rather than running off the end of the table"
        );
        // The row is on every screen, so a terminal that has said it cannot
        // draw `·`/`ω` gets the sentence this line used to say — not tofu,
        // which would indicate nothing at all.
        let bare = Moment {
            caps: crate::caps::Caps {
                unicode: false,
                ..crate::caps::Caps::default()
            },
            ..Moment::default()
        }
        .working();
        let plain =
            Status::render(&State::default(), &Viewport::new(Rect::sized(70, 1), &bare))[0].plain();
        assert!(plain.contains("运行中"), "{plain:?}");
        assert!(
            !plain.contains("(=^"),
            "not a cat it cannot draw: {plain:?}"
        );
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
    }

    #[test]
    fn every_mood_and_phase_is_well_formed() {
        // 4 moods × 12 phases, all asserted. Animation is exhaustively testable
        // because the frame is a value and the time is injected.
        for mood in [Mood::Idle, Mood::Thinking, Mood::Happy, Mood::Sad] {
            for tick in 0..12u64 {
                let m = Moment::default().at_tick(tick);
                let vp = Viewport::new(Rect::sized(20, 1), &m);
                let lines = Mascot::render(&mood, &vp);
                assert_eq!(lines.len(), 1);
                assert!(lines[0].width() <= 20);
                assert!(!lines[0].plain().is_empty());
            }
        }
    }
}
