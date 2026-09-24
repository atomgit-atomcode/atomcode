//! The ambient state: everything a module needs that the log cannot explain.
//!
//! Deliberately a closed struct rather than a bag. The log is the source of
//! truth for what happened; this is the short list of things that are true
//! *now* and are not facts — terminal size, who has focus, the half-typed line,
//! the scroll position, the time. Enumerating them is what stops
//! non-derivable state from growing quietly: adding a field here is a visible
//! act, and every one of them has to be settable by a test.

use crate::i18n::{t, Msg};
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

/// How long after one Escape a second one still means "open the rewind panel",
/// in milliseconds.
///
/// Shorter than [`QUIT_HINT_MS`], and for the opposite reason: the two-press
/// exit *says* it is armed, so its window can be generous. This one says
/// nothing — it is a double-tap, the way a double-click is — so the window has
/// to be short enough that two unrelated presses a second apart are two
/// presses, not a gesture. 1s is a comfortable "consecutive" window and still
/// well under the pause that means "I stopped, then decided something else".
pub const ESC_AGAIN_MS: u64 = 1_000;

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

/// A paste of at least this many lines folds into a `[Pasted #N …]` marker.
/// The same threshold `atomcode-tuix` uses, so the two screens fold the same
/// pastes.
pub const PASTE_FOLD_LINES: usize = 5;
/// …or at least this many characters (a long single-line paste — a URL, a
/// token — that `+1 lines` would misdescribe).
pub const PASTE_FOLD_CHARS: usize = 400;
/// How long after folding a paste a *second* paste of the same block counts as
/// "expand it in place" rather than a fresh paste.
pub const DOUBLE_PASTE_EXPAND_MS: u64 = 1_000;

/// The paste just folded into a `[Pasted #N …]` marker, kept so a second paste
/// of the same block can expand it in place (the "paste again to see it all"
/// gesture).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentFoldedPaste {
    /// Which paste this is: `[Pasted #id …]`, at index `id - 1` in `pastes`.
    pub id: usize,
    /// Byte offset where the marker starts in `input`.
    pub start: usize,
    /// The exact marker text, so an edited marker is not taken for an untouched
    /// one.
    pub placeholder: String,
    /// When it was folded, against [`Moment::now`].
    pub at: Timestamp,
}

/// `\r\n` and lone `\r` to `\n`. Most terminals separate the lines of a
/// bracketed paste with CR, and without this a twenty-line paste reads as one
/// line to `str::lines()` (so it would never fold) and the model would get
/// CR-only separators.
fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Put every `[Pasted #N …]` marker in `text` back to its body from `pastes`
/// (index `N - 1`). A malformed or out-of-range marker is left exactly as
/// written. Called at submit: the model gets the whole paste, while the composer
/// and the history keep the terse marker.
pub fn expand_pastes(text: &str, pastes: &[String]) -> String {
    if pastes.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[Pasted #") {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find(']') else {
            // An unterminated marker is not a marker — keep the rest verbatim.
            out.push_str(tail);
            return out;
        };
        let header = &tail[..=end];
        let id = header
            .strip_prefix("[Pasted #")
            .and_then(|body| body.split_whitespace().next())
            .and_then(|token| token.parse::<usize>().ok());
        match id {
            Some(id) if id >= 1 && id <= pastes.len() => out.push_str(&pastes[id - 1]),
            _ => out.push_str(header),
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// How much of an allowance window is gone, for the status row.
///
/// The window **nearest its limit**, not all of them: the one that will stop
/// the work first is the only one a glance can act on, and the rest are on the
/// usage page for whoever wants them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allowance {
    /// What the host calls this window — "5 小时", "每周".
    pub label: String,
    /// Whole percent spent, 0..=100.
    pub percent: u8,
    /// Seconds until it comes back. `0` when nothing is waiting.
    pub resets_in_seconds: i64,
}

impl Allowance {
    /// The window nearest its limit, out of what the host answered.
    ///
    /// Pure, and over the contract's own type, so "which window matters" can be
    /// judged without a host or a network. A window the host cannot put a
    /// number on is skipped rather than counted as zero — saying "0% used"
    /// because nobody knew would be inventing an answer.
    pub fn nearest(windows: &[atomcode_host_api::UsageWindow]) -> Option<Self> {
        windows
            .iter()
            .filter_map(|w| w.used_percent.map(|percent| (w, percent)))
            .max_by_key(|(_, percent)| *percent)
            .map(|(w, percent)| Self {
                label: w.label.clone(),
                percent: percent.min(100),
                resets_in_seconds: w.resets_in_seconds,
            })
    }
}

/// The non-derivable half of what a module renders from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Moment {
    pub activity: Activity,
    /// A turn has started but its opening line is not on screen yet. The working
    /// status ("正在等待模型") waits on this: it is armed when the turn starts and
    /// spent when the turn's first fact (the person's own message, normally)
    /// lands, so the spinner never paints a frame ahead of the message that
    /// started the turn. Screen state, not a fact — it never leaves this struct.
    pub pending_working: bool,
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
    /// Whether [`title`](Self::title) was set by the PERSON (via `/rename` /
    /// `/title`) rather than auto-generated from the first prompt. The window
    /// title still carries any name; the name PILL on the input rule shows only a
    /// user-chosen one, so an auto-guess does not pin a chip to the composer.
    pub title_user_set: bool,
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
    /// The `Ctrl+R` reverse search, while one is up. `None` the rest of the
    /// time, which is most of it.
    ///
    /// Here rather than in [`crate::search`] as a static for the reason
    /// everything else on this struct is here: the screen is drawn from one
    /// value, so what is typed into the search and what the composer shows
    /// cannot come to disagree.
    pub search: Option<crate::search::Search>,
    /// Pictures the composer is holding, waiting for the message that carries
    /// them. Not a fact until it is sent: a screenshot attached and then
    /// deleted is a gesture, not a thing that happened.
    pub attachments: crate::attach::Attachments,
    /// The bodies of the big blocks folded into `[Pasted #N …]` markers in
    /// [`input`](Self::input), in marker order (index 0 = paste #1). The
    /// composer shows the terse marker; `expand_pastes` puts the body back at
    /// submit, so the model gets the whole paste. Screen state, cleared when the
    /// draft is sent or dropped.
    pub pastes: Vec<String>,
    /// The paste that was JUST folded, for the "paste it again to expand it"
    /// gesture: a second paste of the same block within a short window, with its
    /// marker still untouched at the caret, swaps the marker back for the body
    /// in place. `None` once anything else is typed.
    pub recent_folded_paste: Option<RecentFoldedPaste>,
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
    /// When something last arrived for the turn in flight, on the same clock as
    /// [`Moment::now`]. `None` between turns.
    ///
    /// A turn's elapsed time says how long it has been going; this says how long
    /// it has been since it last did anything, which is the difference between a
    /// model that is slow and one that has gone quiet. The inter-token budget is
    /// minutes long and a stalled stream is recovered from silently, so without
    /// this the screen has nothing to distinguish the two.
    pub quiet_since: Option<Timestamp>,
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
    /// How hard the agent is asked to think, from its description. `None` is no
    /// opinion — the endpoint's own default stands — which is what a session
    /// that never ran `/effort` has, and the row says nothing about it then
    /// rather than inventing a level nothing was configured with.
    ///
    /// Travels the road [`Moment::ctx_window`] does, and for the same reason:
    /// the thinking level is the agent's, not a fact the conversation records.
    pub effort: Option<atomcode_kernel::provider::ReasoningEffort>,
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
    /// When the last Escape was pressed, while a second one would still mean
    /// "open the rewind panel".
    ///
    /// Screen state and not a fact, the same as [`Moment::quit_armed`] — and a
    /// reading rather than a flag, because this gesture says nothing on screen:
    /// the person who double-taps Esc gets the panel, and the person who pressed
    /// it once and went back to work must not find a panel under their next
    /// stray press. The clock is the only thing that can tell those two apart.
    /// Any other action drops it (`plugin.rs`'s `act`); see
    /// [`Moment::escape_again`].
    pub esc_armed: Option<Timestamp>,
    /// The last prompt this session sent to the model, kept so an Escape that
    /// stops a running turn can hand it back to the composer — you interrupt,
    /// the words you sent are in the field again (caret at the end), ready to
    /// edit and resend. Only the model-bound text (not a slash command), and
    /// only restored when the field is empty at the moment of the stop, so an
    /// Escape never overwrites something you had already started typing.
    pub last_sent: Option<String>,
    /// Whether the last turn ended because you stopped it. Drives the dim
    /// `已中断 · …` line under the composer, and is cleared the moment the next
    /// turn starts — screen state, not a fact, the same as the rest here.
    pub interrupted: bool,
    /// The clipboard as last looked at, and whether a picture there is being
    /// offered. See `crate::clip_hint` for the rules.
    pub clip: crate::clip_hint::ClipHint,
    /// The offer is up: the composer's upper rule says a picture is on the
    /// clipboard and which key takes it. Kept apart from [`clip`](Self::clip)
    /// so a frame is drawn from a flag, not from a clock.
    pub clipboard_hint: bool,
    /// A picture is being turned into text by the VL helper for a non-vision
    /// model, and the turn's first fact has not arrived yet. Drives the live
    /// line's `正在识别图片` while that recognition runs — otherwise the screen is
    /// blank for the seconds it takes. Screen state, cleared the moment the turn
    /// materialises or ends.
    pub recognizing_image: bool,
    /// When [`recognizing_image`](Self::recognizing_image) was raised, on the
    /// same injected clock as [`now`](Self::now) — so the live line can show how
    /// long recognition has been running (a picture-recognition can be slow, and
    /// a stalled one and a working one look identical without a clock). Set at
    /// start; read only while `recognizing_image` is true, so a stale value after
    /// it clears is harmless (the next start overwrites it).
    pub recognizing_since: Option<Timestamp>,
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
    /// The same words as [`steering`](Self::steering), **one entry per
    /// message** — the receipt id it was sent under, and the text as sent
    /// (`[Image #N]` markers included).
    ///
    /// `steering` is what the panel draws, joined by newlines, where a message
    /// with a newline of its own cannot be told from two. The id is what ties a
    /// line to the runtime's answer about it: a stop withdraws what is still
    /// waiting and answers each withdrawn send `Rejected { NotRunning }`, and
    /// only a line named by such an answer is one the model will never get.
    /// Kept and cleared with `steering`.
    pub queued: Vec<(String, String)>,
    /// Lines the runtime withdrew, oldest first, waiting for the turn to be
    /// over so they can be handed back — or, after `Ctrl+B`, sent again.
    ///
    /// Only what was withdrawn: a stop that lost the race to the turn's own
    /// end withdrew nothing, the lines went on to the model, and giving them
    /// back as well would say them twice.
    pub withdrawn: Vec<String>,
    /// The last stop was `Ctrl+B`: what it withdraws goes out again, each as
    /// its own message, rather than back to the composer. `esc` and `ctrl-c`
    /// set it false, so it always says what the latest stop asked for.
    pub resend_withdrawn: bool,
    /// 人自己跑过的 `!` 命令与它们的输出,等着跟下一条消息一起给模型。
    ///
    /// 攒着而不是当场发:跑一条 `!git status` 不是在对模型说话,不该因此开
    /// 一个回合。但接下来那句「按上面那个报错改一下」指的就是它 —— 模型没
    /// 见过的话,那句话就是空的。
    pub pending_context: Vec<String>,
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
    /// The allowance window nearest its limit, as of the last check.
    ///
    /// Polled rather than pushed, and that is forced: an allowance moves on the
    /// account's clock, not on anything this session does, so there is nothing
    /// to subscribe to. The check is made after a turn ends and no more often
    /// than [`crate::plugin::ALLOWANCE_EVERY`] — a question asked on the
    /// account costs a round trip, and the answer only matters at the rate a
    /// person can spend it.
    ///
    /// Kept whatever the figure is, so the decision about *when it is worth
    /// saying* stays in the drawing, where it can be judged without a network
    /// (`crate::modules::status`). `None` is "never got an answer" — a host
    /// that meters nothing, or a check that has not happened yet.
    pub allowance: Option<Allowance>,
    /// How much this session may do without asking, as the host last said.
    ///
    /// Here for the reason [`Moment::autonomy`] is: an execution mode is not a
    /// fact about the conversation — the log records what happened, and a run
    /// under one mode looks the same as a run under another — so a module could
    /// not fold it and might not reach a service. The screen asks once when it
    /// connects (`HostCommand::Mode`) and follows `HostEvent::ModeChanged` from
    /// then on.
    ///
    /// `None` is **"the host has not said"**, and it is a different state from
    /// any mode: a host whose tree governs no execution mode has none to report,
    /// and a front end that drew `ask` for it would be reporting a policy
    /// nobody configured. `Some(Mode::Ask)` is the other thing — a host that
    /// said the session really is asking.
    pub mode: Option<atomcode_host_api::Mode>,
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
    /// The plugins and marketplaces, as the launcher last read them.
    ///
    /// Here for the reason [`Moment::providers`] is: what is on disk under
    /// `plugins/` is not a fact in the log, and `View::render` may not read a
    /// file — so the list travels this road or none. See `crate::plugins`.
    pub plugins: crate::plugins::PluginsView,
    /// The plugins panel, while it is up: which page, what is typed in its
    /// search box, the row the arrows are on, the form in progress, and the job
    /// that is still running out there.
    pub plugins_panel: Option<crate::plugins::Panel>,
    /// The tool catalog, as the host last answered it.
    ///
    /// Here for the same reason the two above are: what the model can call is a
    /// fact of the running tree, not of the log, and `View::render` may not ask
    /// the tree anything — so it travels this road or none. See `crate::tools`.
    pub tools: crate::tools::ToolsView,
    /// The tools panel, while it is up: what is typed in its search box, the
    /// row the arrows are on, what it has to say about the last key, and the
    /// switch that is still on its way there and back.
    pub tools_panel: Option<crate::tools::Panel>,
    /// The turns this session can go back to, as the host last answered it.
    ///
    /// Here for the same reason the four above are: which turns can be rewound
    /// is a fact of the running tree (and of a workspace checkpoint on disk),
    /// not of the log, and `View::render` may not ask the tree anything — so it
    /// travels this road or none. See `crate::rewind`.
    pub rewind: crate::rewind::RewindView,
    /// The rewind panel, while it is up: which turn the arrows are on, which
    /// step it is at, and the rewind that is still on its way there and back.
    pub rewind_panel: Option<crate::rewind::Panel>,
    /// The MCP servers this tree has, as the host last answered it.
    ///
    /// Here for the reason [`Moment::tools`] is: which servers exist, and whether
    /// this project is trusted, is a fact of the running tree — not of the log —
    /// and `View::render` may not ask the tree anything, so it travels this road
    /// or none. See `crate::mcp`.
    pub mcp: crate::mcp::McpView,
    /// The MCP panel, while it is up: which of the two levels it is at, which
    /// server or action the arrows are on, what it has to say about the last key,
    /// and the action that is still on its way there and back.
    pub mcp_panel: Option<crate::mcp::Panel>,
    /// The sessions that can be resumed, as the host last answered `/resume`.
    /// Here for the same reason `rewind` is: which sessions exist is a fact of
    /// the on-disk catalog, not of the log, and `View::render` may not read it —
    /// so it travels this road (filled by the `/resume` command's own trip). See
    /// `crate::resume`.
    pub resume: crate::resume::ResumeView,
    /// The resume panel, while it is up: which session row the arrows are on and
    /// what is typed in its search box.
    pub resume_panel: Option<crate::resume::Panel>,
    /// 选中那个会话最后聊了什么。`None` 是还没问到(或问不出来),
    /// `Some(id, None)` 是正在问,`Some(id, Some(lines))` 是答案。
    ///
    /// 放在这儿而不是面板里:面板是纯数据、每一键重算一次,而这个是异步回来的,
    /// 且要能认出「答案回来时人已经走到别的行上了」——那时它作废。
    pub resume_preview: Option<(String, Option<Vec<String>>)>,
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
    /// **One gate, and it is the person's.** An earlier draft also required
    /// `caps.unicode`, on the reasoning that a dot a terminal draws as a tofu
    /// box is worse than no dot. That was the wrong bit: `Caps::unicode`
    /// answers "can this terminal draw *decorative* Unicode" — box drawing,
    /// `✓`, `▸` — which is a question about the cell grid. A title is not
    /// drawn on the grid at all; the tab bar and the window manager draw it,
    /// and whether *they* have an emoji font is something this program is
    /// never told.
    ///
    /// Warp is where that conflation showed: it sets `TERM=dumb` for its shell
    /// integration, so `Caps::detect` reads the terminal as ASCII — while the
    /// very same terminal draws `┏━┓` panels and renders the dot perfectly.
    /// The light was configured on and silently withheld.
    ///
    /// The escape hatch for a tab bar that really does show tofu is the
    /// setting itself, which is what it is documented for.
    pub fn status_dot_on(&self) -> bool {
        self.settings
            .rows()
            .iter()
            .find(|row| row.id == crate::settings::STATUS_DOT)
            .is_none_or(|row| row.value == "true")
    }

    /// Whether plain Tab cycles the execution mode, rather than reserving itself
    /// for the completion menu (`ui.mode_switch_key = "tab"`).
    ///
    /// `false` for no row at all, on the same terms [`Moment::status_dot_on`]
    /// keeps: a launcher with no settings port gets this build's default, which
    /// is `shift_tab` — the chord every terminal but the phone can send.
    pub fn mode_switch_on_tab(&self) -> bool {
        self.settings
            .rows()
            .iter()
            .find(|row| row.id == crate::settings::MODE_SWITCH_KEY)
            .is_some_and(|row| row.value == "tab")
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

    /// Arm the double-tap: one Escape has been dealt with, and a second one
    /// within [`ESC_AGAIN_MS`] opens the rewind panel.
    pub fn arm_escape(&mut self) {
        self.esc_armed = Some(self.now);
    }

    /// Drop it. Called for every action other than an Escape, so a press
    /// followed by real work never leaves the next Escape opening a panel
    /// nobody asked for.
    pub fn disarm_escape(&mut self) {
        self.esc_armed = None;
    }

    /// Whether this Escape is the second of a double-tap — the one that pulls
    /// the rewind panel up.
    ///
    /// Read against the injected clock, never `Instant::now()` (`docs/adr/0008`):
    /// the reading is whatever the frame before this press was painted from,
    /// which is exactly the resolution a gesture made of two presses needs.
    pub fn escape_again(&self) -> bool {
        self.esc_armed
            .is_some_and(|at| self.now.as_millis().saturating_sub(at.as_millis()) <= ESC_AGAIN_MS)
    }

    /// Whether a turn is running right now — the question Escape, Ctrl+C and a
    /// steering Submit all ask before they act. `activity` alone is not the whole
    /// answer: a turn that has just started reads `Idle` until its first fact
    /// raises the working line (see `Host::arm_working`), and in that window a
    /// turn is very much in flight. `pending_working` closes that gap, so a stop
    /// gesture the instant after a send still stops.
    pub fn turn_in_flight(&self) -> bool {
        self.activity != Activity::Idle || self.pending_working
    }

    /// Put this project's older history in front of what this session has said,
    /// and move everything that points into the list by as much. `true` when
    /// anything was actually added.
    ///
    /// `older` is oldest-first, the same order [`history`](Self::history) is in.
    /// Lines already present are dropped rather than duplicated: this session's
    /// own lines are in the project's logs too, and a history that showed the
    /// last thing twice would make Up feel broken.
    ///
    /// **One method rather than three statements at the call site**, and that is
    /// the whole point of it: the list grows at the *front*, so every index into
    /// it is wrong afterwards — the browsing position and the search's hit both.
    /// Two of those were once two lines next to each other, which is a shape
    /// where adding a third index means remembering to come back here. Nobody
    /// remembers. Now the arithmetic and the indices live together and a
    /// judgement can hold them to it.
    pub fn history_grew_older(&mut self, older: Vec<String>) -> bool {
        let already: std::collections::HashSet<&String> = self.history.iter().collect();
        let mut older: Vec<String> = older
            .into_iter()
            .filter(|line| !already.contains(line))
            .collect();
        if older.is_empty() {
            return false;
        }
        let grew = older.len();
        if let Some(at) = self.history_at.as_mut() {
            *at += grew;
        }
        if let Some(search) = self.search.as_mut() {
            search.shift_by(grew);
        }
        older.append(&mut self.history);
        self.history = older;
        true
    }

    /// Insert a pasted block at the caret. A block past the fold threshold
    /// ([`PASTE_FOLD_LINES`]/[`PASTE_FOLD_CHARS`]) folds into a `[Pasted #N …]`
    /// marker — the body kept in [`pastes`](Self::pastes), the composer left
    /// terse — while a *second* paste of the same block, its marker untouched at
    /// the caret and within [`DOUBLE_PASTE_EXPAND_MS`], expands it in place
    /// instead. A small paste goes in raw.
    ///
    /// `now` is threaded in (not read from a clock) so the double-paste window
    /// stays testable, the same as everything else that needs the wall time.
    pub fn insert_paste(&mut self, text: &str, now: Timestamp) {
        let text = normalize_newlines(text);
        if self.expand_recent_folded_paste(&text, now) {
            return;
        }
        self.recent_folded_paste = None;
        let line_count = text.lines().count().max(1);
        let char_count = text.chars().count();
        let at = self.caret.min(self.input.len());
        if line_count >= PASTE_FOLD_LINES || char_count >= PASTE_FOLD_CHARS {
            let id = self.pastes.len() + 1;
            // A single long line uses `{N} chars`; `+1 lines` would misdescribe a
            // 600-char URL. Multi-line uses `+{M} lines`, what a code block reads as.
            let placeholder = if line_count <= 1 {
                format!("[Pasted #{id} {char_count} chars]")
            } else {
                format!("[Pasted #{id} +{line_count} lines]")
            };
            self.pastes.push(text);
            self.input.insert_str(at, &placeholder);
            self.caret = at + placeholder.len();
            self.recent_folded_paste = Some(RecentFoldedPaste {
                id,
                start: at,
                placeholder,
                at: now,
            });
        } else {
            self.input.insert_str(at, &text);
            self.caret = at + text.len();
        }
    }

    /// Attach `image` and drop its `[Image #N]` marker in at the caret. The
    /// marker is part of the sentence, so it lands where the caret is rather than
    /// appended, and a space is added before it when the caret sits on a word.
    ///
    /// The one place a picture joins the composer — a clipboard chord and a
    /// pasted image-file path both come through here, so numbering and caret
    /// handling can never drift between them.
    pub fn insert_image(&mut self, image: atomcode_kernel::message::ImageContent) {
        let label = self.attachments.add(image);
        let at = self.caret.min(self.input.len());
        // Safe on the caret's invariant, the same one `DeleteWord` relies on:
        // every writer of `self.caret` leaves it on a character boundary.
        #[allow(
            clippy::string_slice,
            reason = "the caret is kept on a character boundary by every writer of it"
        )]
        let head = &self.input[..at];
        let gap = if !head.is_empty() && !head.ends_with(char::is_whitespace) {
            " "
        } else {
            ""
        };
        let inserted = format!("{gap}{label}");
        self.input.insert_str(at, &inserted);
        self.caret = at + inserted.len();
    }

    /// The "paste again to expand it" gesture: a second paste of the block that
    /// was just folded, its marker still whole at the caret and within the
    /// window, swaps the marker back for the body. `true` when it did.
    fn expand_recent_folded_paste(&mut self, incoming: &str, now: Timestamp) -> bool {
        let Some(recent) = self.recent_folded_paste.clone() else {
            return false;
        };
        let within =
            now.as_millis().saturating_sub(recent.at.as_millis()) <= DOUBLE_PASTE_EXPAND_MS;
        let end = recent.start + recent.placeholder.len();
        let untouched = self.caret == end
            && self
                .input
                .get(recent.start..end)
                .is_some_and(|current| current == recent.placeholder);
        // Only the newest paste can un-fold this way, and only if its body is
        // exactly what came in again — an edited registry or a different block
        // is a fresh paste, not an expansion.
        let backing = recent.id == self.pastes.len()
            && self
                .pastes
                .get(recent.id.saturating_sub(1))
                .is_some_and(|body| body == incoming);
        if !within || !untouched || !backing {
            return false;
        }
        self.input.replace_range(recent.start..end, incoming);
        self.caret = recent.start + incoming.len();
        self.pastes.pop();
        self.recent_folded_paste = None;
        self.history_at = None;
        true
    }

    /// Drop the folded-paste bookkeeping — after the draft is sent, or dropped.
    pub fn clear_pastes(&mut self) {
        self.pastes.clear();
        self.recent_folded_paste = None;
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
        // Emptying the field also leaves history browsing: the shoulder badge
        // (`历史 N/M`) is drawn from `history_at`, so a cleared field left with a
        // stale browsing position keeps a count for words that are no longer
        // there. Drop the set-aside draft and the folded-paste side-tables too —
        // the field is going to nothing, not back to what was being edited, and
        // orphaned paste bodies would otherwise linger. `search` is torn down
        // upstream before Ctrl+C reaches here (an unhandled key ends the search
        // in `crate::search::key`), so nulling it is belt-and-suspenders.
        self.history_at = None;
        self.search = None;
        self.draft.clear();
        self.clear_pastes();
        self.quit_armed = true;
        self.notice =
            Some(Notice::for_ms(t(Msg::MomentQuitAgain), false, self.now, QUIT_HINT_MS).below());
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

    /// A stop gesture (Escape / Ctrl+C) and a steering Submit all ask
    /// `turn_in_flight`, and a turn is in flight the instant it is armed — before
    /// its first fact raises the working line, when `activity` still reads `Idle`.
    /// Without the `pending_working` half, an Escape in that window would find
    /// nothing to stop.
    #[test]
    fn a_turn_is_in_flight_the_instant_it_is_armed() {
        let mut m = Moment::default();
        assert!(!m.turn_in_flight(), "idle, nothing armed");
        m.pending_working = true;
        assert!(
            m.turn_in_flight(),
            "armed but not yet Working is still a running turn"
        );
        m.pending_working = false;
        m.activity = Activity::Working;
        assert!(m.turn_in_flight());
        m.activity = Activity::Stopping;
        assert!(m.turn_in_flight());
    }

    #[test]
    fn ctrl_c_while_browsing_history_clears_the_badge_state() {
        // Arrow-up put a recalled line in the field and a `历史 N/M` badge on the
        // shoulder; a first Ctrl+C empties the field and must leave browsing too,
        // or the badge lingers over an empty composer (the reported bug).
        let mut m = Moment {
            history: vec!["one".into(), "two".into(), "three".into()],
            draft: "half-typed".into(),
            ..Default::default()
        };
        // A folded paste from before browsing must not survive an empty field.
        m.insert_paste(&big(200), Timestamp::millis(0));
        assert!(
            !m.pastes.is_empty(),
            "the big paste folded into a side-table"
        );
        // Then arrow-up into the history (set the browsing position last so the
        // paste above cannot be what clears it).
        m.history_at = Some(1);
        assert!(!m.cancel_idle(), "first press arms, does not quit");
        assert_eq!(m.input, "");
        assert_eq!(m.history_at, None, "history browsing is left");
        assert!(m.search.is_none());
        assert_eq!(m.draft, "", "the set-aside draft is dropped, not restored");
        assert!(
            m.pastes.is_empty(),
            "folded-paste bookkeeping is dropped too"
        );
        assert!(m.recent_folded_paste.is_none());
    }

    fn big(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_small_paste_goes_in_raw() {
        let mut m = Moment::default();
        m.insert_paste("hi there", Timestamp::millis(0));
        assert_eq!(m.input, "hi there");
        assert!(m.pastes.is_empty(), "nothing folded");
        assert_eq!(m.caret, "hi there".len());
    }

    #[test]
    fn older_history_goes_in_front_and_takes_every_index_with_it() {
        let mut m = Moment {
            history: vec!["mine one".into(), "mine two".into()],
            // browsing the newest of this session's two
            history_at: Some(1),
            ..Default::default()
        };
        assert!(m.history_grew_older(vec!["older one".into(), "older two".into()]));
        assert_eq!(
            m.history,
            vec!["older one", "older two", "mine one", "mine two"],
            "the project's older lines go in FRONT: the list is oldest-first"
        );
        assert_eq!(
            m.history_at,
            Some(3),
            "and what was being browsed is still what is being browsed"
        );
    }

    #[test]
    fn a_line_this_session_already_said_is_not_listed_twice() {
        let mut m = Moment {
            history: vec!["cargo fmt".into()],
            ..Default::default()
        };
        assert!(m.history_grew_older(vec!["git log".into(), "cargo fmt".into()]));
        assert_eq!(m.history, vec!["git log", "cargo fmt"]);
        // And an answer that adds nothing says so, so the caller can skip the
        // repaint rather than redraw the same list.
        assert!(!m.history_grew_older(vec!["cargo fmt".into()]));
    }

    #[test]
    fn a_multi_line_paste_folds_to_a_lines_marker() {
        let mut m = Moment::default();
        let body = big(PASTE_FOLD_LINES);
        m.insert_paste(&body, Timestamp::millis(0));
        assert_eq!(m.input, format!("[Pasted #1 +{PASTE_FOLD_LINES} lines]"));
        assert_eq!(m.pastes, vec![body.clone()]);
        // And submit puts the whole thing back.
        assert_eq!(expand_pastes(&m.input, &m.pastes), body);
    }

    #[test]
    fn a_long_single_line_paste_folds_to_a_chars_marker() {
        let mut m = Moment::default();
        let url = "x".repeat(PASTE_FOLD_CHARS);
        m.insert_paste(&url, Timestamp::millis(0));
        assert_eq!(m.input, format!("[Pasted #1 {PASTE_FOLD_CHARS} chars]"));
        assert_eq!(expand_pastes(&m.input, &m.pastes), url);
    }

    #[test]
    fn crlf_pastes_normalise_and_count_their_lines() {
        let mut m = Moment::default();
        // Five CR-separated lines must count as five (fold), not one.
        let body = "a\r\nb\r\nc\r\nd\r\ne";
        m.insert_paste(body, Timestamp::millis(0));
        assert_eq!(m.input, "[Pasted #1 +5 lines]");
        assert_eq!(expand_pastes(&m.input, &m.pastes), "a\nb\nc\nd\ne");
    }

    #[test]
    fn expand_puts_several_pastes_back_and_leaves_prose_alone() {
        let pastes = vec!["FIRST".to_string(), "SECOND".to_string()];
        assert_eq!(
            expand_pastes(
                "see [Pasted #1 +9 lines] and [Pasted #2 +2 lines] ok",
                &pastes
            ),
            "see FIRST and SECOND ok"
        );
        // An out-of-range or malformed marker is left exactly as written.
        assert_eq!(
            expand_pastes("[Pasted #9 +1 lines]", &pastes),
            "[Pasted #9 +1 lines]"
        );
        assert_eq!(expand_pastes("nothing here", &pastes), "nothing here");
    }

    #[test]
    fn pasting_the_same_block_again_expands_it_in_place() {
        let mut m = Moment::default();
        let body = big(6);
        m.insert_paste(&body, Timestamp::millis(0));
        assert_eq!(m.input, "[Pasted #1 +6 lines]");
        // Second paste of the same block, marker untouched, within the window.
        m.insert_paste(&body, Timestamp::millis(500));
        assert_eq!(m.input, body, "the marker was swapped for the body");
        assert!(m.pastes.is_empty(), "the folded body was taken back");
        assert!(m.recent_folded_paste.is_none());
    }

    #[test]
    fn a_second_paste_after_the_window_folds_again_instead_of_expanding() {
        let mut m = Moment::default();
        let body = big(6);
        m.insert_paste(&body, Timestamp::millis(0));
        // Same block, but too late — a fresh paste, folded as #2.
        m.insert_paste(&body, Timestamp::millis(DOUBLE_PASTE_EXPAND_MS + 1));
        assert_eq!(m.input, "[Pasted #1 +6 lines][Pasted #2 +6 lines]");
        assert_eq!(m.pastes.len(), 2);
    }

    #[test]
    fn a_second_paste_after_typing_does_not_expand_the_marker() {
        let mut m = Moment::default();
        let body = big(6);
        m.insert_paste(&body, Timestamp::millis(0));
        // A keystroke moved the caret off the marker's end — the double-paste
        // gesture is off, so the same block folds again.
        m.caret = 0;
        m.insert_paste(&body, Timestamp::millis(100));
        assert!(m.input.contains("[Pasted #2 +6 lines]"), "{}", m.input);
        assert_eq!(m.pastes.len(), 2);
    }

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

    /// The setting is the gate, and the terminal's `unicode` bit is not.
    ///
    /// The regression this pins: an earlier draft also required
    /// `caps.unicode`, which reads "can this terminal draw *decorative*
    /// Unicode" off `TERM`. Warp sets `TERM=dumb`, so the light was withheld on
    /// the one terminal it was reported missing on — while that same terminal
    /// drew box-drawing panels fine. A title is drawn by the tab bar, not on
    /// the cell grid, so the grid's capability bit has no say in it.
    #[test]
    fn the_light_is_governed_by_the_setting_and_not_by_the_terminals_grid() {
        use crate::settings::STATUS_DOT;
        use crate::text::Light;
        let mut m = Moment::default().working();
        assert_eq!(m.status_dot(), Some(Light::Busy), "on by default");

        // A terminal the grid says is ASCII — Warp, `TERM=dumb` — still gets
        // the light, because the tab bar is not the grid.
        m.caps.unicode = false;
        assert!(
            m.status_dot_on(),
            "an ASCII-grid terminal must not lose the title light"
        );
        assert_eq!(m.status_dot(), Some(Light::Busy));

        // The person's answer is the one that counts, both ways.
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

    /// Two Escapes inside the window are one gesture: the second one is what
    /// pulls the rewind panel up.
    #[test]
    fn a_second_escape_inside_the_window_is_the_gesture() {
        let mut m = Moment {
            now: Timestamp::millis(1_000),
            ..Moment::default()
        };
        assert!(!m.escape_again(), "一下不是手势");
        m.arm_escape();
        m.now = Timestamp::millis(1_000 + ESC_AGAIN_MS);
        assert!(m.escape_again(), "窗口之内的第二下就是那个手势");
    }

    /// Past the window it is a fresh first tap. The gesture says nothing on
    /// screen, so a pause has to be the thing that ends it — otherwise an Esc
    /// pressed a minute ago would still be half a gesture.
    #[test]
    fn a_second_escape_after_the_window_is_a_fresh_first_tap() {
        let mut m = Moment {
            now: Timestamp::millis(1_000),
            ..Moment::default()
        };
        m.arm_escape();
        m.now = Timestamp::millis(1_001 + ESC_AGAIN_MS);
        assert!(!m.escape_again(), "停顿之后,下一下又是第一下");
    }

    /// Anything else between them ends it too — that is what `act` calls when
    /// the action is not an Escape.
    #[test]
    fn work_between_two_escapes_ends_the_gesture() {
        let mut m = Moment {
            now: Timestamp::millis(1_000),
            ..Moment::default()
        };
        m.arm_escape();
        m.disarm_escape();
        assert!(!m.escape_again());
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
