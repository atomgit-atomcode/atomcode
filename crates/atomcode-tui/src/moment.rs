//! The ambient state: everything a module needs that the log cannot explain.
//!
//! Deliberately a closed struct rather than a bag. The log is the source of
//! truth for what happened; this is the short list of things that are true
//! *now* and are not facts — terminal size, who has focus, the half-typed line,
//! the scroll position, the time. Enumerating them is what stops
//! non-derivable state from growing quietly: adding a field here is a visible
//! act, and every one of them has to be settable by a test.

use atomcode_harness::seams::Question;

use crate::frame::Rect;

/// Injected time. Never `Instant::now()` inside a render — see `docs/adr/0008`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// Milliseconds since the session started, as the host counts them.
    pub const fn millis(ms: u64) -> Self {
        Self(ms)
    }
    pub const fn as_millis(self) -> u64 {
        self.0
    }
}

/// What the agent is doing, as the screen needs to know it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Activity {
    #[default]
    Idle,
    Working,
    Stopping,
}

/// Another agent running under this one: a team member, a delegated task.
///
/// Live truth from the agent registry, never a fact in this log — a member is
/// created, starts a turn and is stopped without this conversation committing
/// anything, which is exactly the kind of state [`Moment`] is for. What the
/// screen may show of a member is that it exists, what it is doing, and what
/// it has told this agent (docs/adr/0016): its own conversation is its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemberNow {
    /// The name the lead gave it — the last segment of its session id.
    pub name: String,
    pub activity: Activity,
    /// Which turn it is on, in its own log.
    pub turn: u64,
    /// Its session id: what switching the screen to it addresses.
    pub session: String,
    /// Gone from the team — stopped — and still there to be looked at: its log
    /// is kept (`docs/adr/0023` §5).
    pub gone: bool,
}

/// How far the stream is scrolled from the bottom, in rendered lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollPos(pub usize);

impl ScrollPos {
    pub const BOTTOM: ScrollPos = ScrollPos(0);
    pub fn is_at_bottom(self) -> bool {
        self.0 == 0
    }
}

/// A selection on screen, in cells.
///
/// Anchored where the button went down, headed where the pointer is now.
/// Line-wise rather than rectangular, because that is what every terminal does
/// and what reading a wrapped paragraph needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (u16, u16),
    pub head: (u16, u16),
}

impl Selection {
    pub fn at(x: u16, y: u16) -> Self {
        Self {
            anchor: (x, y),
            head: (x, y),
        }
    }

    /// Nothing covered yet — a press that has not become a drag.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The two ends in reading order. Dragging upward selects the same text as
    /// dragging downward over it.
    fn ends(&self) -> ((u16, u16), (u16, u16)) {
        let (a, h) = (self.anchor, self.head);
        if (a.1, a.0) <= (h.1, h.0) {
            (a, h)
        } else {
            (h, a)
        }
    }

    /// The half-open cell range this row contributes, if it is in the
    /// selection at all. The head's own cell is included: a person who dragged
    /// over a character selected it.
    pub fn on_row(&self, y: u16, width: u16) -> Option<(u16, u16)> {
        let ((sx, sy), (ex, ey)) = self.ends();
        if y < sy || y > ey {
            return None;
        }
        let start = if y == sy { sx } else { 0 };
        let end = if y == ey { (ex + 1).min(width) } else { width };
        (start < end).then_some((start, end))
    }
}

/// How long a [`Notice`] is shown for, in milliseconds.
///
/// Three seconds: long enough to read a short line without hunting for it, short
/// enough that it is gone before it becomes something to clear. It is a constant
/// rather than a per-call argument because a tip that lasted longer in one place
/// than another would be a second lifetime nobody chose.
pub const NOTICE_MS: u64 = 3_000;

/// Something the screen has to say for a moment and then stop saying.
///
/// Transient by construction: the reading it stops at travels with the text, so
/// drawing one is still a pure function of injected state — `render` compares
/// two numbers it was handed rather than reading a clock, which is the rule the
/// whole crate is built on (docs/adr/0008). An expiry the module had to compute
/// itself would be a second clock in the tree, and the live line would drift
/// from the tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    /// On the same clock as [`Moment::now`]. Drawn while `now < until`, gone
    /// from `until` on.
    pub until: Timestamp,
    /// It could not be done. Shown differently, for the same reason
    /// `content::CommandSaid` distinguishes them: "here is your answer" and "I
    /// could not do that" must never look the same.
    pub refused: bool,
}

impl Notice {
    /// One that has `for_ms` of the session's own clock left to live.
    pub fn for_ms(text: impl Into<String>, refused: bool, now: Timestamp, for_ms: u64) -> Self {
        Self {
            text: text.into(),
            until: Timestamp::millis(now.0.saturating_add(for_ms)),
            refused,
        }
    }

    /// Whether it still has something to say at `now`.
    pub fn is_live(&self, now: Timestamp) -> bool {
        now < self.until
    }
}

/// The non-derivable half of what a module renders from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Moment {
    pub activity: Activity,
    /// The line being typed. Not a fact until it is sent.
    pub input: String,
    /// Byte offset of the caret within `input`.
    pub caret: usize,
    pub focus: Option<String>,
    pub scroll: ScrollPos,
    /// What the pointer has selected, if anything. Screen state, not a fact —
    /// which is exactly what this struct is for.
    pub selection: Option<Selection>,
    /// Everything the person has said this session, oldest first.
    ///
    /// Folded from the log rather than appended at submit, so a resumed session
    /// can arrow back through what was said before it was resumed — the log is
    /// the only thing that survives, and a second copy would drift from it.
    pub history: Vec<String>,
    /// Which entry is being shown, when arrowing through them. `None` means
    /// what is in the field is the person's own draft.
    pub history_at: Option<usize>,
    /// The draft that was set aside to go browsing, so leaving the history
    /// gives it back rather than losing it.
    pub draft: String,
    /// Pictures the composer is holding, waiting for the message that carries
    /// them. Not a fact until it is sent: a screenshot attached and then
    /// deleted is a gesture, not a thing that happened.
    pub attachments: crate::attach::Attachments,
    /// Logical frame counter. Animation phase comes from here, never from a
    /// clock read inside `render` — that would make the whole test loop
    /// non-deterministic while leaving it green.
    pub tick: u64,
    /// Injected wall time, for anything that needs a real interval (a
    /// countdown). Still injected: tests set it, `render` never reads a clock.
    pub now: Timestamp,
    /// When the turn in flight began, on the same clock as [`Moment::now`].
    /// `None` between turns.
    ///
    /// Read off a fact rather than derived from one: the log records no clock,
    /// so a turn's start time exists only here — stamped where facts land
    /// (`Host::absorb`), which is the one place that sees both the fact and the
    /// reading. A duration on screen is therefore the difference of two
    /// injected readings, and `render` still never reads a clock
    /// (docs/adr/0008). Folded rather than stored per module for the same
    /// reason [`Moment::history`] is: it is not derivable, and two modules
    /// folding it separately would eventually give two answers.
    pub turn_started: Option<Timestamp>,
    /// Where the agent is working. Not derivable from the log, which is
    /// exactly what this struct is for.
    pub cwd: String,
    /// The agents running under this one, as the registry has them now.
    /// Empty for a screen that never delegates, which is most of them.
    pub members: Vec<MemberNow>,
    /// What the terminal can draw. Injected for the same reason as the clock:
    /// a module that read `TERM` would be right on the developer's machine and
    /// silently wrong on the user's. The surface detects once; everything above
    /// is handed the answer.
    pub caps: crate::caps::Caps,
    /// What the row above the field is saying for a moment, if anything.
    ///
    /// Here rather than in the tip module's folded state because it is exactly
    /// what this struct is for: it is true of *now* and is not a fact — nothing
    /// in the log is a tip, and copying text to the clipboard commits nothing.
    /// It carries its own expiry so the module draws it without a clock.
    pub notice: Option<Notice>,
    /// The mounted cell-grid bitmaps, **as of the frame this moment was taken
    /// for**.
    ///
    /// A snapshot rather than a handle: the rasters in it are immutable, so two
    /// renders against one `Moment` see one picture — the promise `caps` and
    /// `cwd` keep, and the reason a bitmap cannot tear mid-frame.
    ///
    /// It arrives through `Moment` because it has to: `View::render` takes
    /// `&State` and a `&Viewport`, so a module cannot reach a service or a
    /// shared table of its own (`module.rs` says why at length). The host puts
    /// the frame's snapshot here and the module reads it — the same road
    /// `Moment::members` travels. See `docs/adr/0027` decision ①.
    pub rasters: crate::raster::RastersView,
    /// What the person has said while a turn was running, and the model has not
    /// been handed yet — oldest first, joined by newlines.
    ///
    /// Not in the log, and that is not an implementation detail: a message typed
    /// mid-turn is folded in at the next *round* boundary, so until then no
    /// `UserMessage` fact exists to fold. The words are in the agent's inbox and
    /// nowhere else, which is exactly what this struct is for. The front end
    /// appends on submit and clears on `AgentEvent::Steered`; see
    /// `modules::steering`.
    pub steering: String,
    /// The question on screen, if one is waiting, and which of its answers is
    /// pointed at.
    ///
    /// Here for the reason [`Moment::steering`] is: a question is not a fact
    /// until it is answered — the log records the answer, not the asking — and
    /// a module that folded "is a question waiting?" from facts would be folding
    /// something that is not in them. See `modules::ask`.
    pub asking: Option<Ask>,
    /// The session this screen follows: the lead, when there is a team.
    pub lead: String,
    /// The session on screen — the lead, or one of its members the person
    /// switched to (`docs/adr/0023` §3). Everything drawn is this one's.
    pub viewing: String,
    /// The turns an undo, a rewind or an interruption took back
    /// (`docs/adr/0024` §17). What they said stays on screen — the stream is not
    /// reversible — drawn as one dim line each.
    pub undone: std::collections::BTreeSet<u64>,
    /// The team panel's pointed-at row, while the panel has the keyboard.
    ///
    /// One owner for the reason [`Ask::cursor`] has one: the row the arrows are
    /// on and the row the pointer is over are the same row. An index into
    /// `modules::team::targets`.
    pub team_cursor: Option<usize>,
}

/// A question on screen, with the row that is pointed at.
///
/// The cursor lives here rather than in the module's folded state because the
/// highlight has exactly one owner: the row the up/down arrows are on and the
/// row the pointer is over are the same row. Kept in two places, a panel ends
/// up pointing at two rows at once — and the row a click would take has to be
/// the row that is lit, or the light is a lie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    pub question: Question,
    /// Which answer is pointed at, as an index into `question.options`.
    pub cursor: usize,
}

impl Ask {
    /// A question with its first answer pointed at.
    pub fn new(question: Question) -> Self {
        Self {
            question,
            cursor: 0,
        }
    }

    /// Point at `row`, clamped to the answers there are.
    ///
    /// Clamped rather than rejected: a pointer on the panel's last row of
    /// padding, or an arrow pressed past the end of a two-answer question, means
    /// the nearest answer, not nothing.
    pub fn point_at(&mut self, row: usize) -> bool {
        let last = self.question.options.len().saturating_sub(1);
        let row = row.min(last);
        if row == self.cursor {
            return false;
        }
        self.cursor = row;
        true
    }

    /// The value a confirm would return, if there is one to return.
    pub fn picked(&self) -> Option<String> {
        self.question
            .options
            .get(self.cursor)
            .map(|a| a.value.clone())
    }
}

impl Moment {
    pub fn working(mut self) -> Self {
        self.activity = Activity::Working;
        self
    }
    pub fn typing(mut self, text: impl Into<String>) -> Self {
        self.input = text.into();
        self.caret = self.input.len();
        self
    }
    /// Set who is running under this agent. Every field of this struct has to
    /// be settable by a test, or it is not really injected state.
    pub fn with_members(mut self, members: Vec<MemberNow>) -> Self {
        self.members = members;
        self
    }
    pub fn at_tick(mut self, tick: u64) -> Self {
        self.tick = tick;
        self
    }
    /// Say something for a moment, at `now`. The same call the host makes, so a
    /// test exercises the real expiry rather than one it wrote itself.
    pub fn with_notice(mut self, text: impl Into<String>, refused: bool, now: Timestamp) -> Self {
        self.now = now;
        self.notice = Some(Notice::for_ms(text, refused, now, NOTICE_MS));
        self
    }
}

/// What one module is given to draw into.
///
/// Carries the rect and the ambient state, and nothing else — no `Context`, no
/// services, no IO. A module that needs data must fold it from facts, which is
/// what makes "screen-visible is logged" true rather than aspirational.
#[derive(Clone, Copy, Debug)]
pub struct Viewport<'a> {
    pub rect: Rect,
    pub moment: &'a Moment,
}

impl<'a> Viewport<'a> {
    pub fn new(rect: Rect, moment: &'a Moment) -> Self {
        Self { rect, moment }
    }
    pub fn width(&self) -> u16 {
        self.rect.w
    }
    pub fn height(&self) -> u16 {
        self.rect.h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_moment_is_constructible_without_a_terminal() {
        let m = Moment::default().working().typing("hal").at_tick(7);
        assert_eq!(m.activity, Activity::Working);
        assert_eq!(m.input, "hal");
        assert_eq!(m.caret, 3);
        assert_eq!(m.tick, 7);
        assert!(m.scroll.is_at_bottom());
    }
}
