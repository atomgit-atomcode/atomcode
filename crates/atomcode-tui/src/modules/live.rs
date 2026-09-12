//! The live line: what this turn is doing, for how long, and for how much.
//!
//! One row above the composer, drawn only while a turn is in flight — with a
//! blank row above it, so it is welded neither to the words above nor to the
//! field's rule: the reserved row the composer keeps under it (`modules::tip`)
//! is blank and there either way, so it does not need a margin of its own. It
//! answers a question the conversation cannot: a block that has not arrived yet
//! is not on screen, and the status line says where the session is, not what it
//! is doing this second — so a turn that is thinking, or waiting on a model that
//! has gone quiet, looks like a turn that has finished.
//!
//! Two sources, and the split is the point:
//!
//! * **What it is doing** is folded from the log, like every other module's
//!   state, so a live session, a replay and a resumed one cannot disagree.
//! * **How long it has been doing it** cannot be folded — the log records no
//!   clock (`docs/adr/0008`). The opening reading and the current one are both
//!   in [`Moment`], put there by the host, and `render` subtracts them. It still
//!   never reads a clock, which is what keeps the test loop deterministic.
//!
//! Nothing is invented. A figure with no fact behind it is left out rather than
//! printed as zero: no reading yet means no number, not `0 tok`.
//!
//! The figures are counted the two ways the facts demand, and the line is where
//! both are visible at once: `入` and `缓存` are the *latest* request's own
//! readings — the context is re-sent whole every round, so summing would count
//! the same tokens once per round — while `出` is summed over the turn, because
//! each round's output is new. `耗时` is neither: it is the difference of two
//! readings the host injected.
//!
//! Its place on screen is written once, by `host::composer`, rather than claimed
//! with `LayoutOp::Show` the way the mascot and the team strip claim theirs: it
//! is part of the composer, and `Show`'s two sides — above the conversation or
//! below the status bar — are both wrong for a row whose whole job is to sit
//! against the input box.

use atomcode_harness::session::SessionEvent;

use crate::caps::{Glyph, SPINNER};
use crate::el::El;
use crate::frame::{Line, Style};
use crate::module::{Height, View};
use crate::moment::{Activity, Moment, Viewport};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "live";

/// What the turn in flight is doing, as the facts so far say.
///
/// Ordered by nothing but which fact moved it: a request opens it, chunks say
/// whether reasoning or prose is coming back, calls say it is running tools, and
/// the last result coming home means the next thing to arrive is the model's
/// answer to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// A request is out and nothing has come back yet.
    #[default]
    Waiting,
    /// Reasoning is streaming.
    Thinking,
    /// The answer is streaming.
    Writing,
    /// Calls are out and their results are not all back.
    Tools,
}

#[derive(Default)]
pub struct State {
    /// The turn in flight — `None` between turns. Opened by `TurnStart` and
    /// closed by `TurnEnd`, so the line is about what is happening rather than
    /// about the last thing that happened.
    turn: Option<u64>,
    phase: Phase,
    /// Calls issued and not yet answered.
    running: u32,
    /// Output tokens this turn's own readings reported.
    ///
    /// Summed, unlike the context: every round generates new output, while each
    /// request re-sends a prefix of the same input — the trap `TurnStats` in
    /// `content` spells out.
    output: u32,
    /// The context the turn's **last** reading sent, and the part of it that came
    /// from cache.
    ///
    /// Last one wins rather than summed, for the reason `output` is the other way
    /// round: request four re-sends requests one to three, so adding them up
    /// counts the same tokens four times and reports a number about nothing.
    /// Kept as a pair because the share is only a fact about the request they
    /// were both read off: a ratio folded from two rounds' readings would be a
    /// percentage of nothing anyone asked for.
    prompt: u32,
    cached: u32,
    /// Which request of the turn is in flight.
    step: u32,
}

pub struct Live;

impl View for Live {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(state: &mut State, fact: &SessionEvent) {
        match fact {
            // A turn is a budget, and this line is about *this* one. Resetting
            // here is what stops the last turn's step and figures from being
            // read as this turn's.
            SessionEvent::TurnStart { turn } => {
                *state = State {
                    turn: Some(*turn),
                    ..State::default()
                };
            }
            SessionEvent::TurnEnd { .. } => state.turn = None,

            SessionEvent::StepStart { step, .. } => state.step = *step,
            SessionEvent::RequestHeader { .. } => state.phase = Phase::Waiting,
            SessionEvent::AssistantChunk { reasoning, .. } => {
                state.phase = if *reasoning {
                    Phase::Thinking
                } else {
                    Phase::Writing
                };
            }
            SessionEvent::AssistantMessage { tool_calls, .. } => {
                if !tool_calls.is_empty() {
                    state.running = state.running.saturating_add(tool_calls.len() as u32);
                    state.phase = Phase::Tools;
                }
            }
            SessionEvent::ToolResultLogged { .. } => {
                state.running = state.running.saturating_sub(1);
                if state.running == 0 {
                    state.phase = Phase::Waiting;
                }
            }

            // Guarded by the turn, not taken on trust: a reading that arrived
            // after the next turn opened would otherwise land on its line, and
            // nobody reading that number could tell whose work it was.
            SessionEvent::Usage { turn, usage, .. } if state.turn == Some(*turn) => {
                state.output = state.output.saturating_add(usage.completion);
                // Overwritten, not added to: this is what the *latest* request
                // sent. The pair moves together or the share is nonsense.
                state.prompt = usage.prompt;
                state.cached = usage.cached;
            }
            _ => {}
        }
    }

    /// The whole line, or nothing at all.
    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let Some(words) = doing(state, vp.moment) else {
            return Vec::new();
        };

        let muted = theme::fg(Role::Muted);
        let frame = SPINNER[(vp.moment.tick as usize) % SPINNER.len()];
        let sep = format!(" {} ", vp.moment.caps.g(Glyph::Separator));

        let head = format!("{frame} {words}");
        let mut used = width::str_width(&head);
        let mut row: Vec<El> = vec![El::styled(head, style_of(vp.moment))];

        // The words and the clock first, then the figures — and nothing is cut
        // in half. A part that does not fit is left out whole: a timer clipped
        // to `1` reads as a second, and `出 8` reads as a number nobody
        // measured, so which figure fits is decided before drawing rather than
        // by the last cell of the row.
        for part in parts(state, vp.moment) {
            let need = width::str_width(&sep) + width::str_width(&part);
            if used + need > w as usize {
                break;
            }
            used += need;
            row.push(El::styled(sep.clone(), muted));
            row.push(El::styled(part, muted));
        }

        // The margin, only when the rect can hold it: at a height that cannot
        // seat the blank row as well as the words, the words win and the line
        // closes back onto its neighbours. The host clips to `height`, so
        // emitting a blank row it would keep in place of the words is the one
        // way this could draw a live line that says nothing.
        //
        // Above the words only. Under them is the reserved row the composer
        // keeps (`modules::tip`) — blank, and there whether or not this line is
        // mounted — so a margin below would be a second row of nothing between
        // the words and the field's rule, and the two rows would disagree the
        // day a tip is written into one of them.
        let margin = if vp.rect.h >= ROWS {
            MARGIN as usize
        } else {
            0
        };
        let mut out: Vec<Line> = Vec::with_capacity(ROWS as usize);
        for _ in 0..margin {
            out.push(Line::empty());
        }
        out.extend(El::row(row).lay(w));
        out
    }

    /// The line and its margin while a turn is in flight, nothing between turns.
    ///
    /// `Hug(0)` is what hands the row back to the conversation, and it is asked
    /// from the same predicate `render` draws from: the two disagreeing is
    /// either a blank row of chrome above the composer or a line drawn outside
    /// the rect it asked nothing for.
    fn height(state: &State, moment: &Moment, _width: u16) -> Height {
        match doing(state, moment) {
            Some(_) => Height::Hug(ROWS),
            None => Height::Hug(0),
        }
    }

    /// The spinner needs frames, and so does the clock: a timer that only moves
    /// when a fact arrives freezes in exactly the case this line exists for — a
    /// model that has gone quiet. The same cadence as the status line, whose
    /// period the host is already taking the minimum of.
    fn tick() -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(110))
    }
}

/// The blank row above the line while a turn is in flight — the padding that
/// keeps it off the prose.
///
/// Above only: under the line is the composer's [reserved row](super::tip),
/// which is blank and is there whether or not this line is mounted. A margin
/// below would be a second row of nothing between the words and the field's
/// rule, and the two would disagree the day a tip is written into the copy that
/// is not this module's.
///
/// The margin is the module's rather than the composer's, and the reason is the
/// one the composer's own comment gives: a `gap` on that flex is counted between
/// its children whether or not this one is mounted, so between turns the
/// composer would be standing on a blank row — the chrome this module exists to
/// not be. Asked for here, the blank row arrives and leaves with the line it
/// belongs to, in `height`, from the same predicate `render` draws from.
const MARGIN: u16 = 1;

/// What the line asks for in total: the blank row above and the words.
const ROWS: u16 = 1 + MARGIN;

/// What the line says is happening, or `None` when nothing is.
///
/// Two sources, both needed. The log says *what* a turn is doing; the agent's
/// live status says *whether it is still doing it*. The second one earns its
/// place in the case the log cannot report: a resumed session whose process died
/// inside a call leaves a turn open with nothing running, and the facts alone
/// would have this row ticking away at a turn that ended with the process. Every
/// other row shows a record of something that happened; this one is a claim
/// about now, so it asks whoever owns now.
fn doing(state: &State, moment: &Moment) -> Option<String> {
    state.turn?;
    match moment.activity {
        Activity::Idle => None,
        Activity::Stopping => Some("正在停止".to_string()),
        Activity::Working => Some(match state.phase {
            Phase::Waiting => "正在等待模型".to_string(),
            Phase::Thinking => "正在思考".to_string(),
            Phase::Writing => "正在回复".to_string(),
            // `max(1)`: `Tools` is only set with calls outstanding, so a zero
            // here would be a fold that lost one — and "正在运行 0 个工具" is a
            // sentence with no meaning.
            Phase::Tools => format!("正在运行 {} 个工具", state.running.max(1)),
        }),
    }
}

/// The figures, most important first.
///
/// The order is what a person does with them: the clock answers "is it stuck",
/// the two token counts answer "is it getting anywhere", the cache share answers
/// "is the context being re-sent or re-read", and the step number is context for
/// all of them. That order is also the drop order at a narrow width — the last
/// one is the first to go.
///
/// The step is left out at 1 because the turn opening *is* the first step —
/// `第 1 步` on every turn would be a figure that never changes until suddenly it
/// does.
///
/// Two of these are readings of the **last** request and two are sums over the
/// turn, which is not an inconsistency but the only honest way to count them:
/// `入` and `缓存` describe a context that is re-sent whole every round (so the
/// latest reading *is* the current one), while `出` is work each round did once.
/// A sum of `入` would be a number nothing measured, and a last-only `出` would
/// throw away what the turn has produced. `耗时` is neither: it is a difference
/// of two readings the host injected.
fn parts(state: &State, moment: &Moment) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(ms) = elapsed(state, moment) {
        out.push(format!("耗时 {}", short(ms)));
    }
    if state.prompt > 0 {
        out.push(format!("入 {}", crate::content::token_count(state.prompt)));
    }
    if state.output > 0 {
        out.push(format!("出 {}", crate::content::token_count(state.output)));
    }
    if let Some(hit) = crate::content::cache_hit_rate(state.cached, state.prompt) {
        out.push(format!("缓存 {hit}"));
    }
    if state.step > 1 {
        out.push(format!("第 {} 步", state.step));
    }
    out
}

/// How long the turn has been running, when both readings exist.
///
/// `None` when there is no turn, or when this screen did not see it open: a
/// session attached mid-turn has no opening reading, and inventing one — zero,
/// or the age of the session — would report a duration nobody measured.
fn elapsed(state: &State, moment: &Moment) -> Option<u64> {
    state.turn?;
    let started = moment.turn_started?;
    Some(moment.now.as_millis().saturating_sub(started.as_millis()))
}

/// A duration as a person says it: seconds, then minutes and seconds, then hours
/// and minutes.
///
/// Never milliseconds and never a float: this is read at a glance by someone
/// deciding whether to interrupt, and `13400ms` is a number to work out rather
/// than an answer.
fn short(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m{:02}s", secs % 60);
    }
    format!("{}h{:02}m", mins / 60, mins % 60)
}

/// The words carry the state's colour; the figures stay quiet.
fn style_of(moment: &Moment) -> Style {
    match moment.activity {
        Activity::Stopping => theme::fg(Role::Error),
        _ => theme::fg(Role::Warning),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::moment::Timestamp;
    use atomcode_harness::seams::StopReason;
    use atomcode_harness::session::HeaderReason;
    use atomcode_kernel::stream::TokenUsage;
    use atomcode_kernel::tool::ToolCall;

    crate::tui_conformance!(view Live as live_conformance);

    /// A screen mid-turn, `ms` into it.
    fn working(state: &State, ms: u64) -> Vec<String> {
        let mut moment = Moment::default().working().at_tick(0);
        moment.now = Timestamp::millis(ms);
        moment.turn_started = Some(Timestamp::millis(0));
        draw(state, &moment, 80, 1)
    }

    fn draw(state: &State, moment: &Moment, w: u16, h: u16) -> Vec<String> {
        let vp = Viewport::new(Rect::sized(w, h), moment);
        Live::render(state, &vp).iter().map(|l| l.plain()).collect()
    }

    /// The line, or an empty string when there is none.
    fn line_at(state: &State, moment: &Moment, w: u16) -> String {
        draw(state, moment, w, 1).join("")
    }

    fn fold(facts: &[SessionEvent]) -> State {
        let mut state = State::default();
        for fact in facts {
            Live::absorb(&mut state, fact);
        }
        state
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: r#"{"file_path":"a.rs"}"#.into(),
        }
    }

    /// One turn, as the log writes it: a request, reasoning, an answer with two
    /// calls, a reading, both results, and the step that closes it.
    fn a_turn() -> Vec<SessionEvent> {
        vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::StepStart { turn: 1, step: 1 },
            SessionEvent::RequestHeader {
                turn: 1,
                round: 1,
                model: "replay".into(),
                reason: HeaderReason::Series,
            },
            SessionEvent::AssistantChunk {
                turn: 1,
                round: 1,
                delta: "hmm".into(),
                reasoning: true,
            },
            SessionEvent::AssistantChunk {
                turn: 1,
                round: 1,
                delta: "Look".into(),
                reasoning: false,
            },
            SessionEvent::AssistantMessage {
                turn: 1,
                round: 1,
                text: "Looking.".into(),
                reasoning: "hmm".into(),
                tool_calls: vec![call("c1"), call("c2")],
            },
            SessionEvent::Usage {
                turn: 1,
                round: 1,
                usage: TokenUsage {
                    prompt: 1200,
                    completion: 80,
                    cached: 400,
                },
            },
            SessionEvent::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "c1".into(),
                content: "fn main() {}".into(),
                is_error: false,
                images: Vec::new(),
            },
            SessionEvent::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "c2".into(),
                content: "no such file".into(),
                is_error: true,
                images: Vec::new(),
            },
            SessionEvent::StepEnd {
                turn: 1,
                step: 1,
                tool_calls: 2,
            },
        ]
    }

    #[test]
    fn nothing_is_drawn_between_turns() {
        // The row goes back to the conversation: a line saying "空闲" is chrome,
        // and the status line already says where this session is.
        let mut state = fold(&a_turn());
        assert!(
            !working(&state, 0).is_empty(),
            "a turn in flight has a line"
        );

        Live::absorb(
            &mut state,
            &SessionEvent::TurnEnd {
                turn: 1,
                stop: StopReason::Stopped,
                error: None,
            },
        );
        assert!(working(&state, 0).is_empty(), "and a finished one does not");
        assert_eq!(
            Live::height(&state, &Moment::default().working(), 80),
            Height::Hug(0),
            "the row is handed back, not left blank"
        );
    }

    #[test]
    fn the_margin_arrives_with_the_line_and_leaves_with_it() {
        // `ROWS` is the module's answer, and this is what makes it the module's
        // business: the blank row is drawn exactly when the words are, so the
        // composer never stands on one between turns.
        let state = fold(&a_turn());
        let mut moment = Moment::default().working().at_tick(0);
        moment.now = Timestamp::millis(3_000);
        moment.turn_started = Some(Timestamp::millis(0));

        let rows = draw(&state, &moment, 80, 2);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0], "", "a blank row above, not welded to the prose");
        assert!(
            rows[1].contains("正在等待模型"),
            "and the words last: what is under them is the composer's reserved \
             row, not this module's margin, so there is no row of its own to \
             give back below: {rows:?}"
        );

        // A rect that cannot seat the margin keeps the words and drops the
        // blank row. The host clips to `height`, so asking for a blank row
        // first would leave a live line that says nothing.
        let short = draw(&state, &moment, 80, 1);
        assert_eq!(short.len(), 1, "{short:?}");
        assert!(short[0].contains("正在等待模型"), "{short:?}");

        // And the height it asks for is the rect it then fills: the row above
        // and the words while a turn is in flight, none otherwise.
        assert_eq!(
            Live::height(&state, &moment, 80),
            Height::Hug(2),
            "the margin is asked for, not seized"
        );
    }

    #[test]
    fn a_turn_that_opened_before_this_screen_never_claims_a_timer() {
        // Attached mid-turn: the facts are there and the opening reading is
        // not. Zero would be a duration nobody measured, so there is none.
        let state = fold(&a_turn());
        let mut moment = Moment::default().working();
        moment.now = Timestamp::millis(9_000);
        moment.turn_started = None;
        let line = line_at(&state, &moment, 80);
        assert!(line.contains("正在等待模型"), "{line}");
        assert!(
            !line.contains("9s"),
            "no second reading to subtract:\n{line}"
        );
    }

    #[test]
    fn the_clock_is_a_difference_of_two_injected_readings() {
        let state = fold(&a_turn());
        assert!(working(&state, 13_000)[0].contains("13s"));
        // A reading that is not later than the turn's opening — a restarted
        // host, a wrapped counter — reads as zero rather than as a duration
        // nobody measured, and never panics.
        assert!(working(&state, 0)[0].contains("0s"));
    }

    #[test]
    fn a_duration_is_said_the_way_a_person_says_it() {
        assert_eq!(short(0), "0s");
        assert_eq!(short(13_400), "13s", "seconds, not milliseconds");
        assert_eq!(short(59_999), "59s");
        assert_eq!(short(65_000), "1m05s");
        assert_eq!(short(3_723_000), "1h02m");
    }

    #[test]
    fn the_line_says_what_the_turn_is_doing_now() {
        // Each fact moves it, in the order a turn produces them. The line is
        // read at a glance, so the words are the assertion — a phase that does
        // not change the words is a phase nobody can see.
        let mut state = State::default();
        Live::absorb(&mut state, &SessionEvent::TurnStart { turn: 1 });
        assert!(
            working(&state, 0)[0].contains("正在等待模型"),
            "a request is out and nothing has come back"
        );

        let turn = a_turn();
        Live::absorb(&mut state, &turn[3]); // reasoning
        assert!(working(&state, 0)[0].contains("正在思考"));
        Live::absorb(&mut state, &turn[4]); // prose
        assert!(working(&state, 0)[0].contains("正在回复"));
        Live::absorb(&mut state, &turn[5]); // two calls out
        assert!(working(&state, 0)[0].contains("正在运行 2 个工具"));
        Live::absorb(&mut state, &turn[7]); // one result back
        assert!(working(&state, 0)[0].contains("正在运行 1 个工具"));
        Live::absorb(&mut state, &turn[8]); // the last one back
        assert!(
            working(&state, 0)[0].contains("正在等待模型"),
            "with the results in, what it waits on is the model"
        );
    }

    #[test]
    fn the_figures_count_this_turn_and_only_this_turn() {
        let mut state = fold(&a_turn());
        assert!(working(&state, 0)[0].contains("出 80"));

        // A second turn opens its own budget: those 80 were the last one's.
        Live::absorb(&mut state, &SessionEvent::TurnStart { turn: 2 });
        let told = working(&state, 0)[0].clone();
        for gone in ["出", "入", "缓存"] {
            assert!(
                !told.contains(gone),
                "a fresh turn has spent nothing: {told}"
            );
        }
    }

    /// The two ways of counting sit next to each other on purpose, and this is
    /// what says which is which: the context is re-sent whole every round, so
    /// only the latest reading describes it, while each round's output is new.
    #[test]
    fn the_context_is_the_last_round_and_the_output_is_the_whole_turn() {
        let mut state = fold(&a_turn());
        assert!(working(&state, 0)[0].contains("入 1200"));
        assert!(working(&state, 0)[0].contains("缓存 33.33%"));

        Live::absorb(
            &mut state,
            &SessionEvent::Usage {
                turn: 1,
                round: 2,
                usage: TokenUsage {
                    prompt: 2_000,
                    completion: 20,
                    cached: 1_500,
                },
            },
        );
        let said = working(&state, 0)[0].clone();
        assert!(
            said.contains("入 2000"),
            "the latest context, not 1200 + 2000: {said}"
        );
        assert!(
            !said.contains("3200"),
            "a sum of contexts counts the same tokens once per round: {said}"
        );
        assert!(said.contains("出 100"), "the output adds up: {said}");
        assert!(
            said.contains("缓存 75.00%"),
            "the share comes off the same reading as the context: {said}"
        );
    }

    /// A provider that says nothing about caching has not told us the context
    /// was uncached: the share is left out rather than printed as zero. Same rule
    /// as the status line's dropped zero counter.
    #[test]
    fn a_provider_that_reports_no_caching_gets_no_share_on_the_line() {
        let mut state = State::default();
        Live::absorb(&mut state, &SessionEvent::TurnStart { turn: 1 });
        Live::absorb(
            &mut state,
            &SessionEvent::Usage {
                turn: 1,
                round: 1,
                usage: TokenUsage {
                    prompt: 900,
                    completion: 12,
                    cached: 0,
                },
            },
        );
        let said = working(&state, 0)[0].clone();
        assert!(said.contains("入 900"), "{said}");
        assert!(said.contains("出 12"), "{said}");
        assert!(!said.contains("缓存") && !said.contains('%'), "{said}");
    }

    #[test]
    fn a_reading_for_the_turn_that_just_ended_does_not_land_on_the_next_one() {
        // The log is ordered, so this should not happen — which is exactly why
        // it is checked rather than assumed. The number it would land on is
        // about work the new turn did not do, and no reader could tell.
        let mut state = fold(&a_turn());
        Live::absorb(
            &mut state,
            &SessionEvent::TurnEnd {
                turn: 1,
                stop: StopReason::Stopped,
                error: None,
            },
        );
        Live::absorb(&mut state, &SessionEvent::TurnStart { turn: 2 });
        Live::absorb(
            &mut state,
            &SessionEvent::Usage {
                turn: 1,
                round: 2,
                usage: TokenUsage {
                    prompt: 2000,
                    completion: 999,
                    cached: 0,
                },
            },
        );
        let told = working(&state, 0)[0].clone();
        assert!(
            !told.contains("999"),
            "turn 1's output on turn 2's line: {told}"
        );
        assert!(
            !told.contains("入") && !told.contains("缓存"),
            "and neither does the context it sent: {told}"
        );
    }

    #[test]
    fn a_session_that_is_stopping_says_so_and_still_says_how_long() {
        // The words come from the handle, the clock from the log's turn: both
        // are true, and neither is dropped for the other.
        let state = fold(&a_turn());
        let mut moment = Moment::default().at_tick(0);
        moment.activity = Activity::Stopping;
        moment.now = Timestamp::millis(4_000);
        moment.turn_started = Some(Timestamp::millis(0));
        let line = line_at(&state, &moment, 80);
        assert!(line.contains("正在停止"), "{line}");
        assert!(line.contains("4s"), "{line}");
    }

    #[test]
    fn the_figures_go_before_the_words_do() {
        // The line exists to answer "is it stuck", so at a width where it all
        // does not fit, what goes is a figure — and the part that is left is
        // still the part that was looked for.
        let state = fold(&a_turn());
        let mut moment = Moment::default().working().at_tick(0);
        moment.now = Timestamp::millis(12_000);
        moment.turn_started = Some(Timestamp::millis(0));

        let roomy = line_at(&state, &moment, 60);
        for want in ["耗时 12s", "入 1200", "出 80", "缓存 33.33%"] {
            assert!(roomy.contains(want), "{want} missing from {roomy}");
        }

        // Room for the words and the clock, not for the first token figure:
        // `⠋ 正在等待模型` is 14 cells, `耗时 12s` takes it to 25, and the next
        // separator and figure would put it at 34.
        let narrow = line_at(&state, &moment, 25);
        assert!(narrow.contains("正在等待模型"), "the words stay: {narrow}");
        assert!(
            narrow.contains("耗时 12s"),
            "and so does the clock: {narrow}"
        );
        assert!(!narrow.contains("入"), "the figure is dropped: {narrow}");
        assert!(!narrow.contains("1200"), "whole, not halved: {narrow}");

        // Narrower than the words alone: nothing to do but clip them.
        let nothing_fits = line_at(&state, &moment, 3);
        assert!(!nothing_fits.is_empty(), "still the words: {nothing_fits}");
        assert!(width::str_width(&nothing_fits) <= 3);
    }

    /// The clock is a figure like the rest, so at a width where it does not fit
    /// it goes whole rather than as a bare number — `12` would read as a
    /// measurement somebody made, and the words are the part that must survive.
    #[test]
    fn the_clock_goes_whole_when_the_row_cannot_hold_it() {
        let state = fold(&a_turn());
        let mut moment = Moment::default().working().at_tick(0);
        moment.now = Timestamp::millis(12_000);
        moment.turn_started = Some(Timestamp::millis(0));
        let line = line_at(&state, &moment, 20);
        assert!(line.contains("正在等待模型"), "{line}");
        assert!(!line.contains("12"), "no half figure: {line}");
        assert!(!line.contains("耗时"), "{line}");
    }

    #[test]
    fn a_step_count_appears_once_the_turn_is_past_its_first_request() {
        let mut state = fold(&a_turn());
        assert!(
            !working(&state, 0)[0].contains("第"),
            "the turn opening is the first step: there is nothing to count"
        );
        Live::absorb(&mut state, &SessionEvent::StepStart { turn: 1, step: 2 });
        assert!(working(&state, 0)[0].contains("第 2 步"));
    }

    #[test]
    fn the_line_is_what_the_status_says_it_is_doing() {
        // Idle is the one that matters: it is what an interrupted session's log
        // looks like, and the line must not stand there waiting for a model
        // that answered before the process died.
        let state = fold(&a_turn());
        let mut moment = Moment::default().working();
        moment.activity = Activity::Idle;
        assert!(line_at(&state, &moment, 80).is_empty());
    }

    #[test]
    fn it_is_never_wider_than_its_rect_nor_different_twice() {
        // The shared suite renders every module at rest, and folded — with the
        // agent idle, which is a line this module does not draw. So the widths
        // this line is actually read at are checked here, mid-turn, with
        // figures: a part that does not fit is left out whole, and a part that
        // is not left out must not run past the rect.
        let state = fold(&a_turn());
        let mut moment = Moment::default().working();
        moment.now = Timestamp::millis(754_000);
        moment.turn_started = Some(Timestamp::millis(0));
        for w in [0u16, 1, 2, 3, 7, 14, 21, 40, 200] {
            let vp = Viewport::new(Rect::sized(w, 1), &moment);
            let lines = Live::render(&state, &vp);
            for line in &lines {
                assert!(
                    line.width() <= w as usize,
                    "{} cells at width {w}: {:?}",
                    line.width(),
                    line.plain()
                );
            }
            assert_eq!(Live::render(&state, &vp), lines, "not deterministic at {w}");
        }
    }
}
