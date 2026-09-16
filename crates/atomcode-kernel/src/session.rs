//! The session vocabulary: the facts a session's log is made of, and the
//! projection of the model's view out of them.
//!
//! Neutral on purpose (`docs/adr/0024` §6). The log itself — the in-memory
//! `SessionLog`, committing, projections kept current — is the harness's; what
//! is written, what a store keeps and what a front end folds is this, so a store
//! that only knows the kernel (`atomcode-capabilities`) and a front end that must
//! not know the harness can both read it.
//!
//! # Why events instead of messages
//!
//! `Vec<Message>` cannot express what a UI and a compactor both need: the raw
//! chunks a renderer replays, the boundary a compactor cut at, the provenance of
//! an injected reminder. [`derive_messages`] projects the model's view *out* of
//! the log; the log keeps the rest.

use serde::{Deserialize, Serialize};

use crate::event::{PolicyIntervention, StopReason};
use crate::message::{ImageContent, Message, MessageMeta, ReasoningBlock};
use crate::stream::TokenUsage;
use crate::tool::ToolCall;

/// Monotonic position in the log. Stable across a session's life; a consumer
/// that has seen up to `n` resumes from `n + 1`.
pub type SeqNo = u64;

/// Why a new request series began. Carried on `RequestHeader` so a cache-aware
/// consumer can tell an append-only round from a prefix-breaking one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeaderReason {
    /// A normal round appended to the existing prefix.
    Append,
    /// A deliberate new series (a fresh turn after compaction, a model switch).
    Series,
}

/// Where an injected message came from. Injection is model-visible, so it is a
/// logged fact like any other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionOrigin {
    /// Said by another agent — a team member reporting to its lead, a lead
    /// steering a member. `from` is the sender's session id, so a resumed log
    /// still says who spoke, whether or not that agent is alive.
    Peer { from: String },
    /// A persistent memory store.
    Memory,
    /// A `<system-reminder>` style runtime note.
    Reminder,
    /// A continuation the harness itself asked for.
    Continuation,
    /// A nudge the harness asked for, whose talking-only answer is not shown.
    /// See `MessageOrigin::Internal` (harness).
    InternalNudge,
    /// A compaction summary standing in for dropped history.
    CompactionSummary,
}

/// Why the harness is telling a person something.
///
/// Distinct from [`InjectionOrigin`]: that one names model-visible context, this
/// one names a fact the *screen* needs and the model must not see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// A rate limit is being waited out.
    RateLimited,
    /// The history overflowed; it was compacted and the request retried.
    OverflowCompacted,
    /// A stream broke after producing real output, which was preserved.
    StreamRecovered,
    /// A retryable provider failure is backing off.
    ProviderRetry,
    /// The answer hit the output-token limit; the model was asked to resume.
    OutputTruncated,
    /// The turn is ending with its answer still cut off: resuming stopped
    /// working, and the person has half of something.
    OutputLeftCutOff,
}

/// One durable fact about a session.
///
/// `#[non_exhaustive]` because adding a fact must not break a consumer that
/// folds over the ones it knows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionEvent {
    TurnStart {
        turn: u64,
    },
    UserMessage {
        turn: u64,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageContent>,
    },
    /// A step opened: one model request plus whatever tools it calls. Several
    /// steps make a turn, and the boundary is what a UI groups on.
    StepStart {
        turn: u64,
        step: u32,
    },
    StepEnd {
        turn: u64,
        step: u32,
        tool_calls: u32,
    },
    /// Opens a model request. Present even when the request fails, so a failed
    /// round is visible in the log rather than inferred from a gap.
    RequestHeader {
        turn: u64,
        round: u32,
        model: String,
        reason: HeaderReason,
    },
    /// A raw stream fragment. Kept verbatim so replay and UI fidelity do not
    /// depend on re-rendering the finished message.
    AssistantChunk {
        turn: u64,
        round: u32,
        delta: String,
        reasoning: bool,
    },
    AssistantMessage {
        turn: u64,
        round: u32,
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        /// Opaque thinking blocks a provider requires echoed back verbatim
        /// (Anthropic `signature` and kin). Model-visible on the wire, so logged:
        /// a resumed thinking session that lost them is rejected by the provider.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning_blocks: Vec<ReasoningBlock>,
        /// What this response cost and where it sat: tokens, timing, the
        /// correlation ids. A sidecar — never rendered into the request — kept
        /// because a store that persists messages rather than events (the native
        /// snapshot) needs it back, and a fact the log dropped cannot be
        /// recovered by any projection of it. Additive on disk: absent reads as
        /// `None`, and an older reader ignores the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<MessageMeta>,
    },
    /// A call got past every gate and is about to run.
    ///
    /// Not derivable from `AssistantMessage`: that fact says the model ASKED
    /// for a call, and it is committed before approval, plan mode, the
    /// workspace gates or a user's own hook have had a say. A driver that
    /// announced a tool as started from it showed every REFUSED call as one
    /// that began and instantly failed — and, worse, showed "writing file…"
    /// and only then asked whether to allow it.
    ///
    /// Carries the whole call because that is what a driver renders, and
    /// because a gate may have rewritten the arguments on the way through
    /// (`updatedInput`): what started is what runs, not what was asked for.
    ToolStarted {
        turn: u64,
        round: u32,
        call: ToolCall,
    },
    ToolResultLogged {
        turn: u64,
        round: u32,
        call_id: String,
        content: String,
        is_error: bool,
        /// Images the tool produced for a vision model to see. Model-visible,
        /// so logged: a picture the model was shown and the log cannot account
        /// for is exactly what the invariant forbids.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageContent>,
    },
    /// Model-visible text the harness added on its own initiative.
    Injected {
        turn: u64,
        text: String,
        origin: InjectionOrigin,
    },
    /// A question was put to the person.
    ///
    /// Screen-visible is logged, and a question was the one thing a screen
    /// showed that it did not get from here: asking went out through the
    /// `user-questions` seam and the answer came back as the tool's return
    /// value, so what the person decided was written on the screen and nowhere
    /// else. That made an approval the single exception to "one screen, one
    /// log" — a remounted panel lost the answer, and a resumed session redrew
    /// the call that was approved with no sign anyone had ever been asked.
    ///
    /// Committed **before** the question is drawn rather than with its answer,
    /// because a question that is waiting is a state a client has to be able to
    /// see: one that connects midway folds the log and finds it, and a session
    /// whose process died mid-question resumes with the question it was stuck
    /// on rather than with a gap. The pair is [`SessionEvent::Answered`].
    ///
    /// Not model-visible, deliberately: what goes to the model is the answer,
    /// and it travels its own way — as the tool's result. Putting the card in
    /// the request would have the model read its own approval as news.
    Asked {
        turn: u64,
        /// The question as it was asked, options and all: "allow once / always
        /// allow / deny" is what makes a bare `deny` mean something.
        question: Question,
    },
    /// That question is closed, with an answer or without one.
    ///
    /// Always committed when the asking returns, including when nobody
    /// answered: [`SessionEvent::Asked`] with no answer after it is a question
    /// still waiting, and one that was refused has to be distinguishable from
    /// one nobody ever got to. `None` here is the refusal the seam's contract
    /// promises every caller must read into it.
    Answered {
        turn: u64,
        /// The answer's [`Answer::value`], or `None` for a
        /// question that was closed without one — declined, cancelled, or
        /// nobody there.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        /// Which front end answered, as it describes itself ("the person at the
        /// terminal", "the connected driver"). A record of what was decided is
        /// worth less without it the moment two clients can answer the same
        /// session — a phone and a terminal are not interchangeable answerers,
        /// and a log that cannot tell them apart cannot say who allowed what.
        by: String,
    },
    /// A compaction boundary: everything at or below `through` is replaced by
    /// `summary` for model purposes. The dropped events stay in the log —
    /// compaction changes the projection, not the history.
    Compacted {
        turn: u64,
        through: SeqNo,
        summary: String,
    },
    Usage {
        turn: u64,
        round: u32,
        usage: TokenUsage,
    },
    /// Tool results at or below `through` are shown to the model as a one-line
    /// stub from here on.
    ///
    /// The other half of compaction, and the half a fold cannot do: a long turn
    /// that no longer fits has nothing *settled* to fold away, and what fills
    /// the window is almost always tool output nobody needs in full any more.
    /// Committed as a fact rather than re-derived per render, so what the model
    /// saw stays what the log says — and so the rewrite is monotonic: a stub is
    /// never re-stubbed, and the prefix cache is invalidated once.
    ToolResultsStubbed {
        turn: u64,
        through: SeqNo,
    },
    /// Something the harness did that a person should know about and the model
    /// should not.
    ///
    /// Screen-visible is logged, for the same reason model-visible is: a state
    /// the screen shows but the log cannot explain is a state a panel cannot be
    /// remounted into and a resumed session cannot reproduce. Printing it to
    /// stderr instead is worse than useless in a full-screen UI — it corrupts
    /// the display it was meant to inform.
    Notice {
        turn: u64,
        /// Named `notice` rather than `kind` because the enum is tagged with
        /// `kind` already — a field by that name would shadow the tag.
        notice: NoticeKind,
        detail: String,
    },
    /// The session was named. A fact rather than a header field because a name
    /// changes: the first-prompt guess, then a model's summary, then whatever
    /// the person typed. The log records each; the newest wins.
    Titled {
        turn: u64,
        title: String,
    },
    /// A rate limit paused the turn: it ended cleanly, and resets later.
    ///
    /// Screen-visible, so logged — the reset time is what a driver draws instead
    /// of a red error, and a resumed session can still say why the turn stopped.
    RateLimitPaused {
        turn: u64,
        pause: RateLimitPause,
    },
    /// The person stopped this turn.
    ///
    /// Model-visible, because what the model is shown next depends on it: a note
    /// that it was interrupted, and — when `undone` — none of the turn's own work.
    /// The facts stay in the log; the projection leaves them out. Committed only
    /// for a person's cancel, not for the harness stopping a turn to reconfigure
    /// or shut down: those do not mean anyone abandoned the request.
    Interrupted {
        turn: u64,
        /// The turn's prompt and partial work no longer reach the model.
        undone: bool,
    },
    /// Everything from `to` up to this fact no longer reaches the model
    /// (`docs/adr/0024` §17): an undo, a rewind of the conversation, a return
    /// to an earlier point. The facts stay in the log — the projection leaves
    /// them out — so nothing is rewritten and a later reader can still see what
    /// was undone.
    ///
    /// `to` is the sequence number of the `TurnStart` of the first turn undone.
    /// Facts about the session rather than the turn stay in: an injected memory
    /// or compaction summary. A compaction that fell inside the range goes with
    /// it, and the projection falls back to the one before.
    Rewound {
        /// The turn this was committed in.
        turn: u64,
        to: SeqNo,
        scope: RewindScope,
    },
    /// A hard boundary ended the turn and the person has to choose how to go on.
    ///
    /// Screen-visible, so logged: the recovery choices ("complete it yourself",
    /// "skip this step") are what a driver draws, and a session resumed after the
    /// process died owes the person the same choice rather than a silent stop.
    /// Not model-visible — the model already read the refusal as the tool's result.
    PolicyIntervention {
        turn: u64,
        intervention: PolicyIntervention,
    },
    TurnEnd {
        turn: u64,
        /// Why it ended. The reason itself, not a rendering of it: a consumer
        /// that has to match on `"Cancelled"` to tell a stop from a failure is
        /// one typo away from calling a failed turn a clean one.
        stop: StopReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl SessionEvent {
    pub fn turn(&self) -> u64 {
        match self {
            Self::TurnStart { turn }
            | Self::UserMessage { turn, .. }
            | Self::StepStart { turn, .. }
            | Self::StepEnd { turn, .. }
            | Self::RequestHeader { turn, .. }
            | Self::AssistantChunk { turn, .. }
            | Self::AssistantMessage { turn, .. }
            | Self::ToolStarted { turn, .. }
            | Self::ToolResultLogged { turn, .. }
            | Self::Injected { turn, .. }
            | Self::Asked { turn, .. }
            | Self::Answered { turn, .. }
            | Self::Compacted { turn, .. }
            | Self::ToolResultsStubbed { turn, .. }
            | Self::Usage { turn, .. }
            | Self::Notice { turn, .. }
            | Self::Titled { turn, .. }
            | Self::PolicyIntervention { turn, .. }
            | Self::Interrupted { turn, .. }
            | Self::Rewound { turn, .. }
            | Self::RateLimitPaused { turn, .. }
            | Self::TurnEnd { turn, .. } => *turn,
        }
    }

    /// Whether this event contributes to what the model sees. The invariant
    /// checker and `derive_messages` must agree on this set.
    pub fn is_model_visible(&self) -> bool {
        matches!(
            self,
            Self::UserMessage { .. }
                | Self::AssistantMessage { .. }
                | Self::ToolResultLogged { .. }
                | Self::Injected { .. }
                | Self::Compacted { .. }
                | Self::ToolResultsStubbed { .. }
                | Self::Interrupted { .. }
        )
    }
}

/// Bumped when the on-disk shape of a session changes in a way a reader must
/// know about. A reader that meets a version it does not know refuses the
/// file rather than guessing.
///
/// **2** — added [`SessionEvent::ToolStarted`]. Additive in Rust (the enum is
/// `#[non_exhaustive]`, so a consumer folding over the facts it knows still
/// compiles) but NOT additive on disk: the enum is `#[serde(tag = "kind")]`
/// and `JsonlStore::parse` propagates a parse error, so an older reader meeting
/// this fact fails the whole file rather than skipping the line. Refusing by
/// version is the same outcome stated honestly, which is what this constant is
/// for. Making the reader skip what it does not understand would be a different
/// trade — "guess" instead of "refuse" — and belongs to whoever owns that call.
///
/// **3** — added [`SessionEvent::Asked`] and [`SessionEvent::Answered`], the
/// same shape of change for the same reason. What a person decided used to
/// exist only on the screen that asked, which is the one thing a log is
/// supposed to be able to redraw.
///
/// **4** — added [`SessionEvent::PolicyIntervention`], `PolicyDenied` as a way a
/// turn ends, [`SessionEvent::Interrupted`], [`SessionEvent::RateLimitPaused`]
/// with `RateLimited` as a way a turn ends, [`NoticeKind::OutputLeftCutOff`],
/// [`SessionEvent::ToolResultsStubbed`] and [`InjectionOrigin::InternalNudge`]. Same shape again: a hard
/// boundary's recovery choice, and what a person's cancel does to the history,
/// were kernel behaviour the log never saw.
pub const SESSION_FORMAT_VERSION: u32 = 5;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What a [`SessionEvent::Rewound`] takes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RewindScope {
    /// The conversation: what the model sees.
    Conversation,
    /// The workspace, restored from a checkpoint. The conversation is untouched.
    Code,
    Both,
}

impl RewindScope {
    pub fn takes_back_conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::Both)
    }
}

/// What is true of a session before its first event, and stays true.
///
/// Not an event, on purpose. Events are the session's *work*, and a fork
/// inherits a prefix of its parent's work — but not its parent's identity.
/// Keeping the header outside the event stream is what lets a fork carry the
/// parent's events under its own name, and what lets `session/list` answer
/// "which sessions, from where, since when" by reading one line per file.
///
/// The mutable facts about a session — its title — are events, because they
/// change and the log is where change is recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    /// Unix milliseconds.
    pub created_at: u64,
    /// The world's root when the session was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The session this one was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// How many of the log's leading events are the parent's work rather than
    /// this session's. Zero for a session that started empty. Persisted so a
    /// resume, a replay and a transcript can all tell the two apart.
    #[serde(default)]
    pub inherited: usize,
    /// The environment block the session's system prompt carried when it
    /// started — working directory, project instructions, a git snapshot.
    /// Kept so a continued session sends the prefix it was sent before, not
    /// one re-rendered from a repository that has moved on since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

impl SessionHeader {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            version: SESSION_FORMAT_VERSION,
            id: id.into(),
            created_at: now_ms(),
            cwd: None,
            parent: None,
            inherited: 0,
            context: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoggedEvent {
    pub seq: SeqNo,
    /// When it was committed: milliseconds since the Unix epoch, from the log's
    /// wall clock. Beside the event rather than in it (`docs/adr/0024` §14) —
    /// the event is what happened, this is when. `0` for a record written
    /// before commit times were kept.
    pub at: u64,
    pub event: SessionEvent,
}

/// A committed fact, broadcast with the session it belongs to.
///
/// The id is not decoration. One process runs more than one log — a delegated
/// child has its own — and every listener registered above them sees all of
/// them, because that is what one-way realm visibility means. Without the id on
/// the broadcast, a subagent's transcript arrives on the parent's screen and in
/// the parent's file, and nothing downstream can tell it apart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Committed {
    /// Which log this fact was appended to.
    pub session: String,
    pub seq: SeqNo,
    /// When it was committed. See [`LoggedEvent::at`].
    #[serde(default)]
    pub at: u64,
    pub event: SessionEvent,
}

impl Committed {
    /// The log record, without the routing information.
    pub fn logged(&self) -> LoggedEvent {
        LoggedEvent {
            seq: self.seq,
            at: self.at,
            event: self.event.clone(),
        }
    }
}

/// Give a run of events new sequence numbers starting at `first`, keeping their
/// order and the references between them.
///
/// The one reference an event makes to another is a compaction's `through`, so
/// that moves with the events it points at; a boundary that pointed below the run
/// keeps pointing below it. For a host that hands a tree a seed for a session the
/// store already holds events under: the store's numbering must stay monotonic,
/// or a compaction boundary written later means a different cut on replay.
pub fn renumber(events: Vec<LoggedEvent>, first: SeqNo) -> Vec<LoggedEvent> {
    let Some(old_first) = events.first().map(|e| e.seq) else {
        return events;
    };
    let map = |seq: SeqNo| {
        if seq < old_first {
            seq
        } else {
            seq - old_first + first
        }
    };
    events
        .into_iter()
        .map(|mut logged| {
            logged.seq = map(logged.seq);
            match &mut logged.event {
                SessionEvent::Compacted { through, .. }
                | SessionEvent::ToolResultsStubbed { through, .. } => *through = map(*through),
                _ => {}
            }
            logged
        })
        .collect()
}

/// The projection, as a free function so it can be tested against a literal log
/// and reused by a persistence layer replaying someone else's events.
pub fn derive_messages(events: &[LoggedEvent]) -> Vec<Message> {
    project(events, false)
}

/// [`derive_messages`], with each assistant message's logged `meta` attached.
///
/// Same projection, one more field. The model's request is built from
/// [`derive_messages`] so that nothing about it changes; this is for a consumer
/// that persists messages and needs the stats back — the native snapshot store.
/// One function behind both, so the two views cannot drift on which events
/// become which messages.
pub fn derive_messages_with_meta(events: &[LoggedEvent]) -> Vec<Message> {
    project(events, true)
}

/// Which turns the conversation still shows: opened after the compaction that
/// stands, not taken back by an undo, not undone after an interruption.
///
/// What is kept beside a log per turn — statistics, display entries — follows
/// this rather than counting messages, so it agrees with [`derive_messages`]
/// about which turns are gone.
pub fn visible_turns(events: &[LoggedEvent]) -> std::collections::BTreeSet<u64> {
    let taken_back = taken_back(events);
    let floor = events
        .iter()
        .filter(|logged| !taken_back(logged.seq))
        .filter_map(|logged| match logged.event {
            SessionEvent::Compacted { through, .. } => Some(through),
            _ => None,
        })
        .next_back()
        .unwrap_or(0);
    let undone: std::collections::HashSet<u64> = events
        .iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::Interrupted { turn, undone: true } => Some(turn),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter(|logged| logged.seq > floor && !taken_back(logged.seq))
        .filter_map(|logged| match logged.event {
            SessionEvent::TurnStart { turn } if !undone.contains(&turn) => Some(turn),
            _ => None,
        })
        .collect()
}

/// What undos took back: every fact from a `Rewound`'s target up to the
/// `Rewound` itself. Several stack.
fn taken_back(events: &[LoggedEvent]) -> impl Fn(SeqNo) -> bool {
    let rewound: Vec<(SeqNo, SeqNo)> = events
        .iter()
        .filter_map(|logged| match &logged.event {
            SessionEvent::Rewound { to, scope, .. } if scope.takes_back_conversation() => {
                Some((*to, logged.seq))
            }
            _ => None,
        })
        .collect();
    move |seq: SeqNo| rewound.iter().any(|(to, at)| seq >= *to && seq < *at)
}

fn project(events: &[LoggedEvent], with_meta: bool) -> Vec<Message> {
    let taken_back = taken_back(events);

    // A compaction boundary replaces everything at or below it. Find the last
    // one first: replaying then discarding would be wasted work and, worse,
    // would let a dropped tool result pair with a surviving call. One that was
    // taken back no longer counts, and the one before it holds again.
    let mut floor: SeqNo = 0;
    let mut summary: Option<&str> = None;
    // How far the stubbing has reached, for the same reason: a result is shown
    // stubbed because a later fact says so.
    let mut stubbed_through: SeqNo = 0;
    for logged in events.iter().filter(|logged| !taken_back(logged.seq)) {
        match &logged.event {
            SessionEvent::Compacted {
                through,
                summary: s,
                ..
            } => {
                floor = *through;
                summary = Some(s);
            }
            SessionEvent::ToolResultsStubbed { through, .. } => {
                stubbed_through = (*through).max(stubbed_through);
            }
            _ => {}
        }
    }
    // Which tool a result came back from, for the stub's first line. The call is
    // logged with the assistant message that asked for it.
    let tool_names: std::collections::HashMap<&str, &str> = events
        .iter()
        .filter_map(|logged| match &logged.event {
            SessionEvent::AssistantMessage { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flatten()
        .map(|call| (call.id.as_str(), call.name.as_str()))
        .collect();

    // Turns the person interrupted and asked to have undone: their own work
    // leaves the projection. Memory and a compaction summary are not the turn's
    // work — they stand for the session — so they stay.
    let undone: std::collections::HashSet<u64> = events
        .iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::Interrupted { turn, undone: true } => Some(turn),
            _ => None,
        })
        .collect();

    let mut messages = Vec::new();
    if let Some(summary) = summary {
        let mut message = Message::system(summary);
        message.synthetic = true;
        messages.push(message);
    }

    for logged in events.iter().filter(|e| e.seq > floor) {
        let session_wide = matches!(
            &logged.event,
            SessionEvent::Injected {
                origin: InjectionOrigin::Memory | InjectionOrigin::CompactionSummary,
                ..
            } | SessionEvent::Interrupted { .. }
        );
        if !session_wide && undone.contains(&logged.event.turn()) {
            continue;
        }
        // An undo takes back a stretch of the log; what stood for the whole
        // session — a memory, a summary — was never the undone turns' to take.
        let stands_for_the_session = matches!(
            &logged.event,
            SessionEvent::Injected {
                origin: InjectionOrigin::Memory | InjectionOrigin::CompactionSummary,
                ..
            }
        );
        if !stands_for_the_session && taken_back(logged.seq) {
            continue;
        }
        match &logged.event {
            SessionEvent::Interrupted { turn, undone } => {
                // Kept: every call the turn asked for has a result, or the next
                // request pairs a call with nothing and a provider rejects it.
                if !undone {
                    let answered: std::collections::HashSet<&str> = events
                        .iter()
                        .filter_map(|e| match &e.event {
                            SessionEvent::ToolResultLogged { call_id, .. } => {
                                Some(call_id.as_str())
                            }
                            _ => None,
                        })
                        .collect();
                    for e in events.iter().filter(|e| e.event.turn() == *turn) {
                        if let SessionEvent::AssistantMessage { tool_calls, .. } = &e.event {
                            for call in tool_calls {
                                if !answered.contains(call.id.as_str()) {
                                    messages.push(Message::tool_result(
                                        &call.id,
                                        "(cancelled)",
                                        true,
                                    ));
                                }
                            }
                        }
                    }
                }
                messages.push(Message::user_interruption());
            }
            SessionEvent::UserMessage { text, images, .. } => {
                if images.is_empty() {
                    messages.push(Message::user(text));
                } else {
                    messages.push(Message::user_with_images(text, images.clone()));
                }
            }
            SessionEvent::Injected { text, origin, .. } => {
                let mut message = match origin {
                    // A continuation speaks as the user, because it is a
                    // prompt; the rest ride as a note the model reads in place.
                    // A nudge is a prompt too — the harness asking for a check it
                    // is owed; what differs is only whether a talking-only answer
                    // is shown (see `MessageOrigin::Internal` (harness)).
                    InjectionOrigin::Continuation | InjectionOrigin::InternalNudge => {
                        Message::user(text)
                    }
                    // A peer's message is something to act on, not a note in
                    // the margin — but it is a report from another agent, not
                    // the person's word, and it says so. Claude Code frames
                    // teammate messages the same way: a peer cannot speak for
                    // the user or grant what only the user can.
                    InjectionOrigin::Peer { from } => Message::user(format!(
                        "[message from {from} — another agent's report, not the user]\n{text}"
                    )),
                    // A runtime note belongs where it happened, not in the
                    // instruction header. Anything that reaches the request as
                    // `Role::System` is lifted to position 0 and coalesced into
                    // the assembled prompt (see `provider::push_system_coalesced`),
                    // so a note added mid-turn would rewrite the prefix of every
                    // request after it and invalidate the whole prefix cache —
                    // while `RequestHeader` still recorded `Append`. Riding as a
                    // user message appends instead, and it is the shape the
                    // providers already handle: Anthropic merges a consecutive
                    // user run (`merge_consecutive_user`, which names this very
                    // case), OpenAI/Ollama tolerate the adjacency.
                    InjectionOrigin::Reminder => Message::user(text),
                    // A summary stands in for the history it replaced, so it is
                    // part of the frozen prefix rather than a note beside it.
                    InjectionOrigin::CompactionSummary | InjectionOrigin::Memory => {
                        Message::system(text)
                    }
                };
                message.synthetic = true;
                messages.push(message);
            }
            SessionEvent::AssistantMessage {
                text,
                reasoning,
                tool_calls,
                reasoning_blocks,
                meta,
                ..
            } => {
                let mut message = Message::assistant(text, tool_calls.clone());
                if !reasoning.is_empty() {
                    message.reasoning = Some(reasoning.clone());
                }
                message.reasoning_blocks = reasoning_blocks.clone();
                if with_meta {
                    message.meta = meta.clone();
                }
                messages.push(message);
            }
            SessionEvent::ToolResultLogged {
                call_id,
                content,
                is_error,
                images,
                ..
            } => {
                let shown = if logged.seq <= stubbed_through {
                    std::borrow::Cow::Owned(build_compact_stub(
                        tool_names.get(call_id.as_str()).copied().unwrap_or("tool"),
                        content,
                        !*is_error,
                    ))
                } else {
                    std::borrow::Cow::Borrowed(content.as_str())
                };
                messages.push(Message::tool_result(call_id, shown.as_ref(), *is_error));
                // A provider serializes images on a user message and rejects
                // them on a tool one, so the picture rides in immediately
                // after the result it belongs to.
                if !images.is_empty() {
                    let mut carrier = Message::user_with_images("", images.clone());
                    carrier.synthetic = true;
                    messages.push(carrier);
                }
            }
            // Chunks, headers, usage and turn boundaries are facts about the
            // session, not content the model receives.
            _ => {}
        }
    }
    messages
}

// ---- questions put to a person ------------------------------------------

/// The three answers an approval can have. The spelling is
/// `atomcode_capabilities::tools::approval`'s, so a decision means the same
/// thing whichever gate asked and whatever carries it.
pub const ANSWER_ALLOW: &str = "allow";
pub const ANSWER_ALWAYS: &str = "allow_always";
pub const ANSWER_DENY: &str = "deny";

/// One answer: what comes back, and what a plain front end prints.
///
/// Serialisable because an answered question is a fact of the session, and a
/// fact is what the log writes down — the card a person answered is not
/// reproducible from the answer alone, and the options are half of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// Returned by `UserQuestions::ask` (harness) when this one is picked.
    pub value: String,
    /// What a front end with nothing better to show prints. A front end that
    /// knows the answer's meaning is free to word it its own way.
    pub label: String,
}

impl Answer {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
        }
    }
    pub fn labelled(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// The call an approval is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AboutCall {
    pub tool: String,
    /// The exact bytes that will run. A front end summarises them for the eye,
    /// but this is what executes — approve what runs, not a paraphrase of it.
    pub arguments: String,
    /// What an `allow_always` would cover, or `None` when this call is not
    /// something to remember. Empty means every call of this tool; otherwise
    /// it is the tool's own scope — `bash` reports the command, so approving
    /// one destructive command never blanket-approves another. Shown, because
    /// a person saying "always" is owed the scope they are saying it to.
    pub grant: Option<String>,
}

/// A question put to a person — data, not a sentence.
///
/// A string was enough while one agent asked and the answers were yes and no.
/// It stopped being enough the moment a delegated member could ask: "allow
/// `write_file`?" with no way to say *who* wants to write is a question a
/// person cannot answer honestly. So everything a front end needs to lay a
/// question out is here, and everything it gets to decide — wording, colour,
/// which key means which answer — is not.
///
/// Serialisable for the same reason: this is the record of what was asked, and
/// the log is where a screen finds it again after a remount or a resume.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// The ask, phrased, for a front end that renders nothing else.
    pub prompt: String,
    /// The answers, in the order they should be offered.
    pub options: Vec<Answer>,
    /// Which agent is asking, when it is not the one the person is driving —
    /// a team member's name. `None` is this conversation itself.
    pub asker: Option<String>,
    /// The call under review, when this is an approval.
    pub about: Option<AboutCall>,
}

impl Question {
    /// A question with nothing behind it: a prompt and some answers.
    pub fn plain(prompt: impl Into<String>, options: &[&str]) -> Self {
        Self {
            prompt: prompt.into(),
            options: options.iter().map(|o| Answer::new(*o)).collect(),
            ..Self::default()
        }
    }

    /// The card an approval shows: the call under review, and the three answers
    /// a risky call can have.
    ///
    /// One constructor for both asking seams, because they are the same card —
    /// the `user-questions` one the interactive policy asks through, and the
    /// handle's own `approval` one a driver round-trips. Two builders that agree
    /// until one changes is how a person ends up with two products' worth of
    /// wording for one decision, and how a log records two different questions
    /// for the same call.
    ///
    /// `grant` is what an `allow_always` would cover, or `None` when the call
    /// may never be remembered — then the option is not offered, because showing
    /// "always allow" for a decision that will be asked again tells the person
    /// something untrue about the permission they just gave. Empty means every
    /// call of this tool.
    pub fn approval(
        tool: &str,
        arguments: &str,
        grant: Option<&str>,
        asker: Option<String>,
    ) -> Self {
        let mut options = vec![Answer::labelled(ANSWER_ALLOW, "allow once")];
        if grant.is_some() {
            options.push(Answer::labelled(ANSWER_ALWAYS, "always allow"));
        }
        options.push(Answer::labelled(ANSWER_DENY, "deny"));
        Self {
            prompt: match &asker {
                Some(who) => format!("Allow `{tool}` to run, asked for by `{who}`?"),
                None => format!("Allow `{tool}` to run?"),
            },
            options,
            asker,
            about: Some(AboutCall {
                tool: tool.to_string(),
                arguments: arguments.to_string(),
                grant: grant.map(str::to_string),
            }),
        }
    }

    /// Just the values, for an asker that only echoes them.
    pub fn values(&self) -> Vec<String> {
        self.options.iter().map(|o| o.value.clone()).collect()
    }
    /// The answer whose value is this, if it is one of them.
    pub fn has(&self, value: &str) -> bool {
        self.options.iter().any(|o| o.value == value)
    }
}

/// Why a rate-limited turn stopped rather than failed, and when to come back.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RateLimitPause {
    pub reset_at_display: String,
    pub reset_label: String,
    #[serde(default)]
    pub secs_until_reset: Option<u64>,
    /// The provider's own reason, for a pause that is not a plan window.
    #[serde(default)]
    pub server_message: Option<String>,
}

/// `build_compact_stub`: `[<tool> ok|FAILED: N lines, first: <≤80 chars>]`. For a bash
/// result whose first line is the `[elapsed: …]` metadata prefix, the SECOND line is used
/// so `first:` surfaces real output, not the exit-code banner.
pub fn build_compact_stub(tool_name: &str, output: &str, success: bool) -> String {
    let line_count = output.lines().count();
    let first_line: String = {
        let mut iter = output.lines();
        let l1 = iter.next().unwrap_or("(empty)");
        let chosen = if l1.starts_with("[elapsed:") {
            iter.next().unwrap_or(l1)
        } else {
            l1
        };
        chosen.chars().take(80).collect()
    };
    let status = if success { "ok" } else { "FAILED" };
    format!("[{tool_name} {status}: {line_count} lines, first: {first_line}]")
}
