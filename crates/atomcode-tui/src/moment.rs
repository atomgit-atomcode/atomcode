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

/// How long the `再按 Ctrl+C 退出` hint below the box stays up — and, because the
/// second press only quits while it shows, how long the two-press exit window is.
///
/// Shorter than [`NOTICE_MS`]: an exit is armed by an accidental keystroke as
/// often as a deliberate one, so the window a stray second press could land in is
/// kept tight. Two seconds is long enough to press again on purpose, short enough
/// that a wandering hand is not left one keystroke from quitting.
pub const QUIT_HINT_MS: u64 = 2_000;

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
    /// Which side of the input box this belongs to. A copy/paste hint sits in the
    /// reserved tip row *above* the field (right-aligned); the `再按 Ctrl+C 退出`
    /// hint sits *below* it, over the status line, the way `atomcode-tuix` and
    /// Claude Code place their exit prompt. The two never show at once, but they
    /// are drawn by different modules — so the module reads this rather than the
    /// wording to decide whether the line is its to draw.
    pub below: bool,
}

impl Notice {
    /// One that has `for_ms` of the session's own clock left to live.
    pub fn for_ms(text: impl Into<String>, refused: bool, now: Timestamp, for_ms: u64) -> Self {
        Self {
            text: text.into(),
            until: Timestamp::millis(now.0.saturating_add(for_ms)),
            refused,
            below: false,
        }
    }

    /// The same notice, drawn below the box rather than in the tip row above.
    pub fn below(mut self) -> Self {
        self.below = true;
        self
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
    /// What this session is called, from the newest `Titled` fact.
    ///
    /// A name changes — the first-prompt guess, then a model's summary, then
    /// whatever somebody typed — so it is read off the log rather than kept as
    /// a header field, and the newest wins. `None` until the session has one.
    pub title: Option<String>,
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
    /// The model the screen agent runs on, from its description — known before
    /// the first turn (which is when the folded status `model` is still empty).
    /// The footer prefers the folded model when it has one and falls back to
    /// this, so a fresh session names the real model instead of the brand. `""`
    /// until the agent has been described.
    pub model: String,
    /// The mounted model's context window, in tokens — the denominator the
    /// status row shows the used-token count against (`49.0k/512k tok`). `0` when
    /// unknown (no model, or a provider that reports none), which the row draws as
    /// a bare `49.0k tok`. Injected from the agent's description, not folded from
    /// the log: the window is the agent's, not a fact the conversation records.
    pub ctx_window: u32,
    /// The agents running under this one, as the registry has them now.
    /// Empty for a screen that never delegates, which is most of them.
    pub members: Vec<MemberNow>,
    /// What the terminal can draw. Injected for the same reason as the clock:
    /// a module that read `TERM` would be right on the developer's machine and
    /// silently wrong on the user's. The surface detects once; everything above
    /// is handed the answer.
    pub caps: crate::caps::Caps,
    /// What a row by the field is saying for a moment, if anything — the tip row
    /// above it for a copy/paste hint, or the status line below it for the exit
    /// hint (see [`Notice::below`]).
    ///
    /// Here rather than in the tip module's folded state because it is exactly
    /// what this struct is for: it is true of *now* and is not a fact — nothing
    /// in the log is a tip, and copying text to the clipboard commits nothing.
    /// It carries its own expiry so the module draws it without a clock.
    pub notice: Option<Notice>,
    /// Whether Ctrl+C on an idle line has been pressed once and is one more press
    /// away from quitting. Screen state, not a fact: it is armed by that first
    /// press (which also clears the line) and disarmed by any other action, so an
    /// accidental press followed by real work never leaves the terminal a
    /// keystroke from exit. It only quits while the paired `再按 Ctrl+C 退出` hint
    /// — a `below` [`Notice`] on [`Moment::notice`] — is still up; see
    /// [`Moment::cancel_idle`] and [`Moment::exit_hint_live`].
    pub quit_armed: bool,
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
    /// The password a running process is blocked on, if one is being asked for:
    /// the asking program's own words, and how many characters have been typed.
    ///
    /// **Never the characters.** This struct is cloned once a frame and read by
    /// anything that draws, so a password in it would be a copy of the password
    /// per frame; the buffer stays in [`crate::secret::Secrets`], which the host
    /// mirrors from. Here for the reason [`Moment::asking`] is — a password is
    /// not a fact, the log records nothing about it, and a module may not reach
    /// a service to ask.
    ///
    /// While it is `Some` the composer draws it in place of the draft, and the
    /// draft is left alone: see [`crate::modules::input`].
    pub secret: Option<crate::secret::Asking>,
    /// What the session is doing on its own, as the host last said.
    ///
    /// Pushed, not polled: the host announces it when a round lands, so a line
    /// that draws from this moves on its own. Here rather than folded from the
    /// log for the reason [`Moment::asking`] is — a goal's round counter is not
    /// a fact about the conversation, it is the state of something running
    /// beside it. `None` is "not driving itself".
    pub autonomy: Option<atomcode_host_api::Running>,
    /// The session this screen follows: the lead, when there is a team.
    pub lead: String,
    /// The session on screen — the lead, or one of its members the person
    /// switched to (`docs/adr/0023` §3). Everything drawn is this one's.
    pub viewing: String,
    /// The turns an undo, a rewind or an interruption took back
    /// (`docs/adr/0024` §17). What they said stays on screen — the stream is not
    /// reversible — drawn as one dim line each.
    pub undone: std::collections::BTreeSet<u64>,
    /// The team panel's pointed-at row: which row the arrows are on while the
    /// panel has the keyboard, and which row the pointer is over regardless.
    ///
    /// An index into `modules::team::targets`. Pointing at a row is what a
    /// pointer does merely by being there, so this is **not** the keyboard: an
    /// ordinary move of the mouse across a panel that is always on screen would
    /// then take the composer's keys away, and the person would be typing into
    /// nothing. Who has the keyboard is [`Moment::team_keyboard`].
    pub team_cursor: Option<usize>,
    /// Whether the team panel has the keyboard (`docs/adr/0023` §3).
    ///
    /// Its own field rather than "there is a cursor", because only one thing
    /// hands the panel the keys — `Tab`, pressed on purpose. The pointer lights
    /// a row and stops there; a stored fact (a question waiting, below) can take
    /// the keyboard without being asked for, but a mouse move is not that.
    pub team_keyboard: bool,
    /// The settings, **as of the frame this moment was taken for**.
    ///
    /// A snapshot rather than a handle, for the reason [`Moment::rasters`] is
    /// one: `View::render` takes `&State` and a `&Viewport` and may not reach a
    /// service, so data a module did not fold from a fact has to arrive through
    /// here. Settings are not facts — nothing in the log records them — so this
    /// is the only road they can travel. See `crate::settings`.
    pub settings: crate::settings::SettingsView,
    /// The settings panel, while it is up: what is typed in its search box, the
    /// row the arrows are on, and the edit in progress.
    ///
    /// Here rather than in the module's folded state for the reason
    /// [`Moment::asking`] is: the panel is not a fact in the log, it is what
    /// *this screen* is doing right now, and a module can neither hold it nor be
    /// reached from outside to be told about it. `None` is a panel that is not
    /// up — the open flag and the state in one field, so a panel that is drawn
    /// and a panel that takes keys cannot disagree.
    pub settings_panel: Option<crate::settings::Panel>,
    /// The providers, as the launcher last read them.
    ///
    /// Here for the reason [`Moment::settings`] is: a provider is not a fact in
    /// the log — nothing about a configuration file is — and `View::render` may
    /// not reach a service, so this is the only road the list can travel. See
    /// `crate::providers`.
    pub providers: crate::providers::ProvidersView,
    /// The providers panel, while it is up: which list, what is typed in its
    /// search box, the row the arrows are on, and the form in progress.
    ///
    /// **Never the API key being typed into that form.** The panel carries how
    /// many characters there are; the characters are the host's, for the reason
    /// [`Moment::secret`] gives — this struct is cloned once a frame and every
    /// module can read it.
    pub providers_panel: Option<crate::providers::Panel>,
    /// What the Usage page draws, as the host last answered it.
    ///
    /// Asked for rather than pushed: an allowance window changes on the
    /// server's clock, not on anything this screen does, so it is fetched when
    /// the page is opened and left alone until it is opened again. `None` is
    /// "not asked yet" and draws as such — an empty list means the host does
    /// not meter, which is a different thing and says so.
    pub usage: Option<crate::settings::UsagePage>,
    /// What the Status page draws. `None` until the host has answered — which
    /// the page says, rather than drawing an empty form.
    pub status: Option<crate::settings::StatusPage>,
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
    /// A password being asked for, with `typed` characters in it. What the host
    /// mirrors from [`crate::secret::Secrets`] — the words and a count, never
    /// the characters — so a test draws the same field a `sudo` produces.
    pub fn asking_password(mut self, prompt: impl Into<String>, typed: usize) -> Self {
        self.secret = Some(crate::secret::Asking {
            prompt: prompt.into(),
            typed,
        });
        self
    }
    /// Set who is running under this agent. Every field of this struct has to
    /// be settable by a test, or it is not really injected state.
    pub fn with_members(mut self, members: Vec<MemberNow>) -> Self {
        self.members = members;
        self
    }
    /// Set the session this screen follows. Separate from [`Moment::viewing`],
    /// which is which of them is drawn: a team panel has a lead whether or not
    /// the lead is the one on screen.
    pub fn with_lead(mut self, lead: impl Into<String>) -> Self {
        self.lead = lead.into();
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

    /// Whether the `再按 Ctrl+C 退出` hint is on screen right now — a below-the-box
    /// notice that has not yet expired. The status line reads this to know whether
    /// to give its row to the hint, and [`cancel_idle`](Self::cancel_idle) reads
    /// it to know whether a second press is the one that quits.
    pub fn exit_hint_live(&self) -> bool {
        self.notice
            .as_ref()
            .is_some_and(|n| n.below && n.is_live(self.now))
    }

    /// What this session is doing, as the light the window title carries.
    ///
    /// Folded from the fields that already say what a turn is doing, rather than
    /// from a counter of its own: a second "is this busy" that can disagree with
    /// the live line is worse than no light at all.
    ///
    /// **A question outranks a turn.** They can both be true — the model asked
    /// and is waiting — and the one worth a red dot is the one a person can do
    /// something about. What is here is what a *module* can see of that: the
    /// question on screen and the password being typed. The ask queue itself is
    /// [`crate::host::Host`]'s, and a question that has not been drawn yet is
    /// folded in by [`crate::host::Host::light`], which is the one the title
    /// uses.
    pub fn light(&self) -> crate::text::Light {
        use crate::text::Light;
        if self.asking.is_some() || self.secret.is_some() {
            return Light::Waiting;
        }
        // `Stopping` is a turn still in flight — the flag it is landing. Busy,
        // so the dot does not go quiet while the thing it is stopping is still
        // running.
        match self.activity {
            Activity::Working | Activity::Stopping => Light::Busy,
            Activity::Idle => Light::Idle,
        }
    }

    /// Whether this screen is allowed to put a light in the window title.
    ///
    /// Two gates, and both have to be open. The person's: the setting, read from
    /// the rows this frame was taken with — an absent row is this build's
    /// default, which is on, so a launcher that never provided a settings port
    /// still gets the light. The terminal's: `caps.unicode`, because a dot a
    /// terminal draws as a tofu box is worse than no dot — and that check has to
    /// happen above the surface, since a title is written whether or not anyone
    /// is watching the screen.
    pub fn status_dot_on(&self) -> bool {
        if !self.caps.unicode {
            return false;
        }
        self.settings
            .rows()
            .iter()
            .find(|row| row.id == crate::settings::STATUS_DOT)
            .is_none_or(|row| row.value == "true")
    }

    /// The light for the window title: what is happening, or `None` when the
    /// person or the terminal has said not to show one.
    pub fn status_dot(&self) -> Option<crate::text::Light> {
        self.status_dot_on().then(|| self.light())
    }

    /// End a pending two-press exit: drop the latch and, with it, the hint below
    /// the box. Called for every action other than a repeat Cancel, so an
    /// accidental first press followed by real work never leaves the terminal one
    /// keystroke from exit — and never leaves the hint up while the work goes on.
    /// A copy/paste notice (`below == false`) is left where it is.
    pub fn disarm_quit(&mut self) {
        self.quit_armed = false;
        if self.notice.as_ref().is_some_and(|n| n.below) {
            self.notice = None;
        }
    }

    /// Apply a Ctrl+C on an idle line. Returns `true` when it is the second press
    /// that quits, `false` when it is the first that arms.
    ///
    /// The first press clears the field and shows the hint below the box; a second
    /// press while that hint is up quits; once the hint has expired the next press
    /// is a fresh first press again.
    pub fn cancel_idle(&mut self) -> bool {
        // A second press quits only while it is still *armed* and the hint is
        // still up: `quit_armed` is dropped by any other action (in `act`), and
        // the hint expires on its own, so either an intervening keystroke or a
        // two-second pause turns the next press back into a fresh first one.
        if self.quit_armed && self.exit_hint_live() {
            return true;
        }
        self.input.clear();
        self.caret = 0;
        self.quit_armed = true;
        self.notice =
            Some(Notice::for_ms("再按 Ctrl+C 退出", false, self.now, QUIT_HINT_MS).below());
        false
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

    /// The light follows what the turn is doing, and a stop still counts as a
    /// turn: the flag has landed on something that is still running.
    #[test]
    fn the_light_follows_the_turn() {
        use crate::text::Light;
        assert_eq!(Moment::default().light(), Light::Idle);
        assert_eq!(Moment::default().working().light(), Light::Busy);
        let stopping = Moment {
            activity: Activity::Stopping,
            ..Moment::default()
        };
        assert_eq!(stopping.light(), Light::Busy);
    }

    /// A question outranks a turn, because it is the one a person can act on —
    /// and every kind of question counts, including a password a process is
    /// blocked on.
    #[test]
    fn a_question_outranks_a_turn_in_flight() {
        use crate::text::Light;
        let mut asking = Moment::default().working();
        asking.asking = Some(Ask::new(Question::plain("which one", &["a", "b"])));
        assert_eq!(asking.light(), Light::Waiting);
        assert_eq!(
            Moment::default()
                .working()
                .asking_password("sudo password", 0)
                .light(),
            Light::Waiting
        );
    }

    /// Both gates: a terminal that cannot draw the dot gets no dot whatever the
    /// setting says, and the setting says off regardless of the terminal.
    #[test]
    fn a_light_needs_both_the_setting_and_a_terminal_that_can_draw_it() {
        use crate::settings::STATUS_DOT;
        use crate::text::Light;
        let mut m = Moment::default().working();
        assert_eq!(m.status_dot(), Some(Light::Busy), "both gates open");

        m.caps.unicode = false;
        assert!(!m.status_dot_on(), "an ASCII terminal gets no dot");
        assert_eq!(m.status_dot(), None);

        m.caps.unicode = true;
        m.settings = crate::settings::SettingsView::new(vec![crate::settings::SettingRow {
            id: STATUS_DOT.to_string(),
            label: "终端状态图标".to_string(),
            value: "false".to_string(),
            kind: crate::settings::SettingKind::Boolean,
            applies: crate::settings::Applies::Immediately,
        }]);
        assert!(!m.status_dot_on(), "the person turned it off");
        assert_eq!(m.status_dot(), None);

        // A launcher that provided no settings port at all is not a "no": the
        // row is absent, so this build's default (on) stands.
        let mut bare = Moment::default().working();
        bare.settings = crate::settings::SettingsView::default();
        assert!(bare.status_dot_on());
    }

    #[test]
    fn cancel_when_idle_clears_the_line_and_arms_the_exit_hint() {
        // The first Ctrl+C on an idle line never quits: it empties the field
        // (whatever was in it) and puts the "再按退出" hint below the box.
        let mut m = Moment::default().typing("half a thought");
        m.now = Timestamp::millis(1_000);
        let quit = m.cancel_idle();
        assert!(!quit, "the first press does not quit");
        assert_eq!(m.input, "", "the line is cleared");
        assert_eq!(m.caret, 0);
        assert!(m.quit_armed, "and armed for the next press");
        assert!(m.exit_hint_live(), "the exit hint is showing");
    }

    #[test]
    fn a_second_cancel_while_the_hint_shows_quits() {
        let mut m = Moment::default().typing("x");
        m.now = Timestamp::millis(1_000);
        assert!(!m.cancel_idle());
        // One reading before the 2s window closes: still armed and hinting.
        m.now = Timestamp::millis(1_000 + QUIT_HINT_MS - 1);
        assert!(m.cancel_idle(), "a second press while the hint shows quits");
    }

    #[test]
    fn a_second_cancel_after_the_hint_expires_re_arms_rather_than_quitting() {
        let mut m = Moment {
            now: Timestamp::millis(1_000),
            ..Default::default()
        };
        assert!(!m.cancel_idle());
        // The window has closed: two seconds later the hint is gone.
        m.now = Timestamp::millis(1_000 + QUIT_HINT_MS);
        let quit = m.cancel_idle();
        assert!(!quit, "an expired hint is a fresh first press, not an exit");
        assert!(m.exit_hint_live(), "and the hint shows again");
    }

    #[test]
    fn work_between_the_two_presses_cancels_the_pending_exit() {
        // `act` drops `quit_armed` on any action other than a repeat Cancel; with
        // it false the still-live hint alone must not be enough to quit.
        let mut m = Moment {
            now: Timestamp::millis(1_000),
            ..Default::default()
        };
        assert!(!m.cancel_idle());
        m.quit_armed = false; // stand-in for "typed something"
        m.now = Timestamp::millis(1_500); // the hint is still live
        assert!(
            !m.cancel_idle(),
            "real work between presses disarms the exit"
        );
    }

    #[test]
    fn disarming_drops_both_the_latch_and_the_exit_hint() {
        // Any other action ends the pending exit: the latch clears and the hint
        // below the box goes with it, rather than hanging on for its two seconds
        // while the person is already doing something else.
        let mut m = Moment {
            now: Timestamp::millis(0),
            ..Default::default()
        };
        m.cancel_idle();
        assert!(m.exit_hint_live());
        m.disarm_quit();
        assert!(!m.quit_armed);
        assert!(!m.exit_hint_live(), "the hint goes when the latch does");
    }

    #[test]
    fn disarming_leaves_a_copy_hint_alone() {
        // A copy/paste notice is not the exit hint; disarming the quit latch must
        // not take it off the tip row above the box.
        let mut m = Moment::default().with_notice("已复制", false, Timestamp::millis(0));
        m.disarm_quit();
        assert!(
            m.notice.is_some(),
            "the copy hint is not the latch's to clear"
        );
    }

    #[test]
    fn a_plain_notice_is_not_the_exit_hint() {
        // The copy/paste hint rides the same field but belongs above the box; it
        // must never read as one press from exit.
        let m = Moment::default().with_notice("已复制", false, Timestamp::millis(0));
        assert!(!m.exit_hint_live(), "a copy hint is not the exit hint");
    }
}
