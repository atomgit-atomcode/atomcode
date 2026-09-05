//! The one-line header: what is running, on what, at what cost.

use atomcode_harness::session::SessionEvent;

use crate::frame::{Color, Line, Style};
use crate::module::{Height, View};
use crate::moment::{Activity, Viewport};
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

    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        use crate::caps::Glyph;
        use crate::el::El;

        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let caps = vp.moment.caps;
        let bar = Style::new().bg(Color::Ansi(236));
        let dim = Style::new().fg(Color::Ansi(245));
        let key = Style::new().fg(Color::Ansi(110));
        let sep = || El::styled(format!(" {} ", caps.g(Glyph::Separator)), dim);

        // Left: who, on what, doing what.
        let mut left = vec![El::styled(
            " atomcode",
            Style::new().fg(Color::Ansi(75)).bold(),
        )];
        if !state.model.is_empty() {
            left.push(sep());
            left.push(El::styled(state.model.clone(), dim));
        }
        left.push(sep());
        left.push(match vp.moment.activity {
            // The phase comes from the injected tick, never a clock — the
            // spinner is a pure function of the frame number (docs/adr/0008).
            Activity::Working => El::styled(
                format!(
                    "{} 运行中",
                    SPINNER[(vp.moment.tick as usize) % SPINNER.len()]
                ),
                Style::new().fg(Color::Ansi(214)),
            ),
            Activity::Stopping => El::styled("停止中", Style::new().fg(Color::Ansi(203))),
            Activity::Idle => match &state.last_stop {
                Some(stop) if stop.contains("Error") => El::styled(
                    format!("{} {stop}", caps.g(Glyph::Fail)),
                    Style::new().fg(Color::Ansi(203)),
                ),
                Some(_) => El::styled(
                    format!("{} 就绪", caps.g(Glyph::Ok)),
                    Style::new().fg(Color::Ansi(114)),
                ),
                None => El::styled("就绪", dim),
            },
        });

        // Right: the numbers, each one only when it means something. A counter
        // reading zero is noise pretending to be information.
        let mut right: Vec<El> = Vec::new();
        let mut chip = |label: &str, value: String| {
            right.push(sep());
            right.push(El::styled(format!("{label} "), dim));
            right.push(El::styled(value, key));
        };
        if state.turns > 0 {
            chip("回合", state.turns.to_string());
        }
        if state.tool_calls > 0 {
            chip("工具", state.tool_calls.to_string());
        }
        if state.prompt_tokens > 0 {
            chip(
                "上下文",
                format!("{}k", (state.prompt_tokens as f32 / 1000.0).round() as u32),
            );
        }
        right.push(El::raw(" "));

        let mut row = left;
        row.push(El::Spacer);
        row.extend(right);
        El::styled_all(bar, El::row(row)).lay(w)
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
            Style::new().fg(Color::Ansi(202)),
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
    fn the_bar_says_what_is_running_and_what_it_cost() {
        let mut st = State::default();
        for fact in [
            SessionEvent::TurnStart { turn: 2 },
            SessionEvent::RequestHeader {
                turn: 2,
                round: 1,
                model: "replay".into(),
                reason: atomcode_harness::session::HeaderReason::Append,
            },
            SessionEvent::StepEnd {
                turn: 2,
                step: 1,
                tool_calls: 2,
            },
        ] {
            Status::absorb(&mut st, &fact);
        }
        let line = Status::render(
            &st,
            &Viewport::new(Rect::sized(70, 1), &Moment::default().working()),
        )[0]
        .plain();
        for expected in ["atomcode", "replay", "回合 2", "工具 2"] {
            assert!(line.contains(expected), "{line:?} is missing {expected}");
        }
        assert_eq!(
            width::str_width(&line),
            70,
            "the bar fills its width exactly, or the background has a hole in it"
        );
    }

    #[test]
    fn a_counter_reading_zero_is_left_out() {
        // A zero is noise pretending to be information.
        let line = Status::render(
            &State::default(),
            &Viewport::new(Rect::sized(70, 1), &Moment::default()),
        )[0]
        .plain();
        assert!(!line.contains("工具"), "{line:?}");
        assert!(!line.contains("回合"), "{line:?}");
    }

    #[test]
    fn usage_takes_the_max_not_the_sum_of_prompt_tokens() {
        // Providers re-send a growing cumulative figure; summing double-counts.
        let mut s = State::default();
        for prompt in [100u32, 900, 900] {
            Status::absorb(
                &mut s,
                &SessionEvent::Usage {
                    turn: 1,
                    round: 1,
                    usage: atomcode_kernel::stream::TokenUsage {
                        prompt,
                        completion: 10,
                        cached: 0,
                    },
                },
            );
        }
        assert_eq!(s.prompt_tokens, 900);
        assert_eq!(s.completion_tokens, 30, "completion really does accumulate");
    }

    #[test]
    fn the_bar_fills_the_width_exactly_at_any_width() {
        let s = State::default();
        let moment = Moment::default();
        for w in 1..80u16 {
            let vp = Viewport::new(Rect::sized(w, 1), &moment);
            let line = &Status::render(&s, &vp)[0];
            assert_eq!(
                line.width(),
                w as usize,
                "a reversed bar must not have gaps"
            );
        }
    }

    #[test]
    fn the_mascot_reacts_to_what_happened() {
        let mut m = Mood::default();
        Mascot::absorb(&mut m, &SessionEvent::TurnStart { turn: 1 });
        assert_eq!(m, Mood::Thinking);
        Mascot::absorb(
            &mut m,
            &SessionEvent::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "c".into(),
                content: "boom".into(),
                is_error: true,
                images: Vec::new(),
            },
        );
        assert_eq!(m, Mood::Sad);
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
