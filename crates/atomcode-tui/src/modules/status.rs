//! The one-line header: what is running, on what, at what cost.

use atomcode_harness::session::SessionEvent;

use crate::frame::{Color, Line, Style};
use crate::module::{Height, View};
use crate::moment::{Activity, Viewport};
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

/// One frame per tick. Braille dots because they are one column everywhere and
/// degrade to a dot rather than to tofu.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

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
        let t = caps.theme;
        let dim = theme::fg(Role::Muted, t);
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
                format!(
                    "{}k tok",
                    (state.prompt_tokens as f32 / 1000.0).round() as u32
                ),
                dim,
            ));
        }
        match vp.moment.activity {
            // The phase comes from the injected tick, never a clock
            // (docs/adr/0008).
            Activity::Working => {
                row.push(sep());
                row.push(El::styled(
                    format!(
                        "{} 运行中",
                        SPINNER[(vp.moment.tick as usize) % SPINNER.len()]
                    ),
                    theme::fg(Role::Warning, t),
                ));
            }
            Activity::Stopping => {
                row.push(sep());
                row.push(El::styled("停止中", theme::fg(Role::Error, t)));
            }
            Activity::Idle => {}
        }
        El::row(row).lay(w)
    }

    fn height(_: &State) -> Height {
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
            Mood::Thinking => &["(=^·^=)", "(=^-^=)", "(=^ω^=)", "(=^-^=)"],
            Mood::Happy => &["(=^▽^=)"],
            Mood::Sad => &["(=；ω；=)"],
        };
        let f = frames[(vp.moment.tick as usize) % frames.len()];
        vec![Line::styled(
            width::take_width(f, vp.rect.w as usize),
            Style::new().fg(Color::role(Role::Error)),
        )]
    }

    fn height(_: &Mood) -> Height {
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
