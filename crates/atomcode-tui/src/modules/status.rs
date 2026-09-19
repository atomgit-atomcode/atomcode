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
    /// The cached part of the last request's context. Read off the same reading
    /// as `prompt_tokens` so the hit rate the row shows (`cache 96%`) is a share
    /// of the request it came from, not two figures from different requests.
    pub cached_tokens: u32,
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
                // The cached share belongs to one request, so it is the last
                // reading rather than a max: a hit rate is only meaningful against
                // the context it was measured on.
                state.cached_tokens = usage.cached;
            }
            SessionEvent::StepEnd { tool_calls, .. } => state.tool_calls += tool_calls,
            SessionEvent::TurnEnd { stop, .. } => state.last_stop = Some(format!("{stop:?}")),
            _ => {}
        }
    }

    /// The bottom line: what model, where, how full the context is, how much of
    /// it was cached — `model │ cwd │ 49.0k/512k tok (10%) │ cache 96%`, the shape
    /// and colours `atomcode-tuix` draws.
    ///
    /// Each part carries its own colour (model = accent, cwd = secondary, the
    /// context usage green→yellow→red as it fills, the cache ratio gold, the
    /// separators muted). It puts this last and lets it end where its text ends —
    /// it is the thing you glance at, not the thing you read. A reverse-video bar
    /// across the top is what an editor does; a coding agent's screen belongs to
    /// the conversation.
    ///
    /// On a narrow terminal the parts drop in tuix's order rather than the whole
    /// row truncating: the cwd shrinks to its project name, then the usage goes,
    /// then the cache, so the model — the one thing you always need — is the last
    /// to be squeezed.
    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        use crate::el::El;

        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let caps = vp.moment.caps;
        let dim = theme::fg(Role::Muted);
        // The status row separates its fields with a vertical bar the way tuix's
        // does (`model │ cwd │ …`), not the middle dot the rest of the screen uses
        // between inline figures — so the fields read as columns. ASCII `|` where
        // the terminal cannot draw box-drawing. Its measured width feeds
        // `fit_status_segments`, so the fitting counts the same gaps the row draws.
        let sep_text = if caps.unicode { " │ " } else { " | " }.to_string();
        let sep_w = width::str_width(&sep_text);

        let mut row: Vec<El> = Vec::new();
        // Width already spoken for by the parts that sit outside the fitted info
        // group: the member prefix ahead of it and the activity indicator after.
        let mut reserved = 0usize;

        // Whose screen this is, when it is a team member's rather than the
        // lead's: everything below and everything typed is that member's.
        let viewing = &vp.moment.viewing;
        if !viewing.is_empty() && *viewing != vp.moment.lead {
            let member = format!("成员 {}", viewing.rsplit('/').next().unwrap_or(viewing));
            reserved += width::str_width(&member) + sep_w;
            row.push(El::styled(member, theme::fg(Role::Accent)));
            row.push(El::styled(sep_text.clone(), dim));
        }

        // The activity indicator is appended after the info group, but its width
        // is reserved now so the group degrades to leave room for it rather than
        // shoving it off the edge. The phase comes from the injected tick, never
        // a clock (docs/adr/0008).
        let activity: Option<(String, Style)> = match vp.moment.activity {
            Activity::Working => Some((working_indicator(vp.moment), theme::fg(Role::Warning))),
            Activity::Stopping => Some(("停止中".to_string(), theme::fg(Role::Error))),
            Activity::Idle => None,
        };
        if let Some((text, _)) = &activity {
            reserved += width::str_width(text) + sep_w;
        }

        let model_str = if state.model.is_empty() {
            "atomcode".to_string()
        } else {
            state.model.clone()
        };
        let cwd_full = vp.moment.cwd.clone();
        let cwd_base = path_basename(&cwd_full).to_string();
        // Only what means something: a context usage segment appears once there
        // are tokens or a window to report, never as a bare zero.
        let ctx_str = if state.prompt_tokens > 0 || vp.moment.ctx_window > 0 {
            format_ctx_usage(state.prompt_tokens as usize, vp.moment.ctx_window as usize)
        } else {
            String::new()
        };
        let cache_str = cache_indicator(state.cached_tokens, state.prompt_tokens).unwrap_or_default();

        let budget = (w as usize).saturating_sub(reserved);
        let segs = fit_status_segments(
            &model_str, &cwd_full, &cwd_base, &ctx_str, &cache_str, budget, sep_w,
        );

        // The context usage shifts green → yellow → red as the window fills toward
        // the auto-compaction threshold, mirroring the reference status bar.
        let ctx_style = match ctx_fill_pct(state.prompt_tokens, vp.moment.ctx_window) {
            p if p >= 90 => theme::fg(Role::Error),
            p if p >= 70 => theme::fg(Role::Warning),
            _ => theme::fg(Role::Success),
        };
        let style_for = |seg: StatusSeg| match seg {
            StatusSeg::Model => theme::fg(Role::Accent),
            StatusSeg::Cwd => theme::fg(Role::Secondary),
            StatusSeg::Ctx => ctx_style,
            StatusSeg::Cache => theme::fg(Role::Warning),
        };
        for (i, (seg, text)) in segs.iter().enumerate() {
            if i > 0 {
                row.push(El::styled(sep_text.clone(), dim));
            }
            row.push(El::styled(text.clone(), style_for(*seg)));
        }

        if let Some((text, style)) = activity {
            row.push(El::styled(sep_text.clone(), dim));
            row.push(El::styled(text, style));
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

/// One colour-differentiated part of the status row's info group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StatusSeg {
    Model,
    Cwd,
    Ctx,
    Cache,
}

/// Last non-empty path segment — the project name shown when a narrow row cannot
/// fit the whole cwd. Splits on both `/` and `\` so a Windows path outside the
/// home dir (which arrives with backslashes) still shrinks to its last segment.
fn path_basename(path: &str) -> &str {
    path.rsplit(|c| c == '/' || c == '\\')
        .find(|seg| !seg.is_empty())
        .unwrap_or(path)
}

/// A token count in the status row's units, `atomcode-tuix`'s `format_tok_count`:
/// `k` below a million and `m` at/above it. `round_clean` keeps a round value
/// clean (`512k`, `1m`) for the window; the used count keeps one decimal
/// (`49.0k`) so it is visibly moving.
fn format_tok_count(n: usize, round_clean: bool) -> String {
    if n >= 1_000_000 {
        if round_clean && n % 1_000_000 == 0 {
            format!("{}m", n / 1_000_000)
        } else {
            format!("{:.1}m", n as f64 / 1_000_000.0)
        }
    } else if n >= 1000 {
        if round_clean && n % 1000 == 0 {
            format!("{}k", n / 1000)
        } else if round_clean {
            format!("{:.0}k", n as f64 / 1000.0)
        } else {
            format!("{:.1}k", n as f64 / 1000.0)
        }
    } else {
        format!("{n}")
    }
}

/// Context usage as `49.0k/512k tok (10%)` when the window is known, or a bare
/// `49.0k tok` when the provider has not reported one yet.
fn format_ctx_usage(used: usize, window: usize) -> String {
    let used_label = format_tok_count(used, false);
    if window == 0 {
        format!("{used_label} tok")
    } else {
        let window_label = format_tok_count(window, true);
        let pct = (used as f64 / window as f64 * 100.0).round() as u64;
        format!("{used_label}/{window_label} tok ({pct}%)")
    }
}

/// How full the window is, as a whole percent, or `0` when the window is unknown.
fn ctx_fill_pct(used: u32, window: u32) -> u64 {
    if window == 0 {
        0
    } else {
        (used as u64).saturating_mul(100) / window as u64
    }
}

/// The cache-hit segment (`cache 96%`), or `None` when the provider reported no
/// caching — a `cache 0%` would state a fact we do not have, the same rule the
/// zero token counter follows.
fn cache_indicator(cached: u32, prompt: u32) -> Option<String> {
    (cached > 0 && prompt > 0).then(|| {
        let pct = (cached as u64 * 100 / prompt as u64).min(100);
        format!("cache {pct}%")
    })
}

/// Joined display width of a segment list, counting the separators (` │ `) drawn
/// between adjacent parts.
fn status_segments_width(segs: &[(StatusSeg, String)], sep_w: usize) -> usize {
    if segs.is_empty() {
        return 0;
    }
    let text: usize = segs.iter().map(|(_, t)| width::str_width(t)).sum();
    text + sep_w * (segs.len() - 1)
}

/// Choose which info-group parts fit within `budget`, degrading in tuix's order
/// for a narrow terminal: shorten the cwd to its project name, then drop the
/// context usage, then drop the cache ratio. The model is always kept; the caller
/// lets `El` truncate it only as an absolute last resort. `sep_w` is the width of
/// the separator `render` draws between parts, so the fitting counts the same
/// gaps the row will.
fn fit_status_segments(
    model: &str,
    cwd_full: &str,
    cwd_base: &str,
    ctx: &str,
    cache: &str,
    budget: usize,
    sep_w: usize,
) -> Vec<(StatusSeg, String)> {
    let build = |cwd: &str, ctx_on: bool, cache_on: bool| {
        let mut v: Vec<(StatusSeg, String)> = Vec::with_capacity(4);
        if !model.is_empty() {
            v.push((StatusSeg::Model, model.to_string()));
        }
        if !cwd.is_empty() {
            v.push((StatusSeg::Cwd, cwd.to_string()));
        }
        if ctx_on && !ctx.is_empty() {
            v.push((StatusSeg::Ctx, ctx.to_string()));
        }
        if cache_on && !cache.is_empty() {
            v.push((StatusSeg::Cache, cache.to_string()));
        }
        v
    };
    let stages = [
        build(cwd_full, true, true),
        build(cwd_base, true, true),   // 1. cwd → project name
        build(cwd_base, false, true),  // 2. drop the usage
        build(cwd_base, false, false), // 3. drop the cache
    ];
    for stage in &stages {
        if status_segments_width(stage, sep_w) <= budget {
            return stage.clone();
        }
    }
    // Nothing fits cleanly — hand back the leanest set; `El` truncates it.
    build(cwd_base, false, false)
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

    /// The reference footer: usage against the window and the cache ratio, in
    /// tuix's `49.0k/512k tok (10%)` / `cache 96%` shape.
    #[test]
    fn the_row_shows_usage_against_the_window_and_the_cache_ratio() {
        let st = State {
            model: "glm5.3-flash-pro".into(),
            prompt_tokens: 49_000,
            cached_tokens: 47_040, // 96% of 49_000
            ..Default::default()
        };
        let m = Moment {
            cwd: "~/Documents/workspace/atomcode".into(),
            ctx_window: 512_000,
            ..Default::default()
        };
        let line = Status::render(&st, &Viewport::new(Rect::sized(120, 1), &m))[0].plain();
        assert!(line.contains("glm5.3-flash-pro"), "{line:?}");
        assert!(line.contains("~/Documents/workspace/atomcode"), "{line:?}");
        // 49000/512000 ≈ 9.57% → 10%.
        assert!(line.contains("49.0k/512k tok (10%)"), "{line:?}");
        assert!(line.contains("cache 96%"), "{line:?}");
    }

    /// With no window reported the usage is a bare count, not `x/0`.
    #[test]
    fn without_a_window_the_usage_is_a_bare_count() {
        let st = State {
            prompt_tokens: 49_000,
            ..Default::default()
        };
        let line = Status::render(&st, &Viewport::new(Rect::sized(120, 1), &Moment::default()))[0]
            .plain();
        assert!(line.contains("49.0k tok"), "{line:?}");
        assert!(!line.contains('/'), "no window means no denominator: {line:?}");
    }

    /// A narrow row shrinks the cwd to its project name and then drops the
    /// figures, in that order — the model is the one thing it never takes away.
    #[test]
    fn a_narrow_row_shrinks_the_cwd_then_drops_the_figures() {
        let st = State {
            model: "glm5.3-flash-pro".into(),
            prompt_tokens: 49_000,
            cached_tokens: 47_040,
            ..Default::default()
        };
        let m = Moment {
            cwd: "~/Documents/workspace/atomcode".into(),
            ctx_window: 512_000,
            ..Default::default()
        };
        let line = Status::render(&st, &Viewport::new(Rect::sized(40, 1), &m))[0].plain();
        assert!(
            line.contains("glm5.3-flash-pro"),
            "the model is never dropped: {line:?}"
        );
        assert!(
            line.contains("atomcode") && !line.contains("workspace"),
            "the cwd shrank to its project name: {line:?}"
        );
    }

    /// Each part carries its own colour: model accent, cwd secondary, the usage
    /// green while the window is far from full, the cache ratio gold.
    #[test]
    fn each_part_of_the_row_carries_its_own_colour() {
        let st = State {
            model: "glm".into(),
            prompt_tokens: 100,
            cached_tokens: 50,
            ..Default::default()
        };
        let m = Moment {
            cwd: "/w".into(),
            ctx_window: 1000,
            ..Default::default()
        };
        let line = &Status::render(&st, &Viewport::new(Rect::sized(120, 1), &m))[0];
        let colour = |needle: &str| {
            line.spans
                .iter()
                .find(|s| s.text.contains(needle))
                .unwrap_or_else(|| panic!("{needle} not on the row: {:?}", line.plain()))
                .style
                .fg
        };
        assert_eq!(colour("glm"), theme::fg(Role::Accent).fg, "model is accent");
        assert_eq!(colour("/w"), theme::fg(Role::Secondary).fg, "cwd is secondary");
        // 100/1000 = 10% < 70, so the usage is green.
        assert_eq!(colour("tok"), theme::fg(Role::Success).fg, "usage is green");
        assert_eq!(colour("cache"), theme::fg(Role::Warning).fg, "cache is gold");
    }

    /// The usage shifts green → yellow → red as the window fills.
    #[test]
    fn the_usage_colour_warns_as_the_window_fills() {
        let colour_at = |pct_used: u32| {
            let st = State {
                model: "m".into(),
                prompt_tokens: pct_used * 10, // out of a 1000-token window
                ..Default::default()
            };
            let m = Moment {
                ctx_window: 1000,
                ..Default::default()
            };
            Status::render(&st, &Viewport::new(Rect::sized(120, 1), &m))[0]
                .spans
                .iter()
                .find(|s| s.text.contains("tok"))
                .expect("the usage segment")
                .style
                .fg
        };
        assert_eq!(colour_at(10), theme::fg(Role::Success).fg, "10% is green");
        assert_eq!(colour_at(75), theme::fg(Role::Warning).fg, "75% is yellow");
        assert_eq!(colour_at(95), theme::fg(Role::Error).fg, "95% is red");
    }

    #[test]
    fn nothing_it_draws_is_wider_than_the_screen() {
        let m = Moment {
            cwd: "/a/very/long/path/that/keeps/going/and/going/and/going".into(),
            // A known window and a running turn too: the fullest the row gets,
            // which is the case that would run off a narrow edge.
            ctx_window: 512_000,
            ..Default::default()
        }
        .working();
        let st = State {
            model: "some-extremely-long-model-name-v2.5-preview".into(),
            prompt_tokens: 123_456,
            cached_tokens: 120_000,
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
