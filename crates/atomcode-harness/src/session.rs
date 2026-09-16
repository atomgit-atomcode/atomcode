//! The session log: an append-only stream of facts, and the projections derived
//! from it.
//!
//! # Model-visible is logged
//!
//! Everything that reaches a model request must be reconstructible from this
//! log. That is not a style preference — it is what makes resume, fork,
//! transcript, telemetry and compaction derivable rather than separately
//! maintained. A new kind of model-visible input therefore means a new
//! [`SessionEvent`] variant, never a side channel the loop reads and the log
//! never saw. [`assert_model_visible_is_logged`] states the invariant, and the
//! loop's tests hold it.
//!
//! # Why events instead of messages
//!
//! `Vec<Message>` cannot express what a UI and a compactor both need: the raw
//! chunks a renderer replays, the boundary a compactor cut at, the provenance of
//! an injected reminder. `derive_messages` projects the model's view *out* of the
//! log; the log keeps the rest.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use atomcode_kernel::message::{ImageContent, Message, MessageMeta, ReasoningBlock};
use atomcode_kernel::stream::TokenUsage;
use atomcode_kernel::tool::ToolCall;
use serde::{Deserialize, Serialize};

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
    /// See [`crate::agent::MessageOrigin::Internal`].
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
    /// A compaction that asks a model has started; the request waits on it.
    Compacting,
    /// A compaction did less than was asked — a summary that timed out fell
    /// back to folding tool output — and the person should know why.
    CompactionDegraded,
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
        question: crate::seams::Question,
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
        /// The answer's [`crate::seams::Answer::value`], or `None` for a
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
    /// A compaction boundary: everything above `from` and at or below `through`
    /// is replaced by `summary` for model purposes. The dropped events stay in the log —
    /// compaction changes the projection, not the history.
    Compacted {
        turn: u64,
        through: SeqNo,
        summary: String,
        /// Events at or below `from` are not folded by this compaction: the head
        /// of the session it keeps — its first request, which every later summary
        /// is measured against. `0` folds from the start. What an earlier
        /// compaction folded stays folded.
        #[serde(default, skip_serializing_if = "is_zero")]
        from: SeqNo,
    },
    Usage {
        turn: u64,
        round: u32,
        usage: TokenUsage,
    },
    /// Tool results at or below `through` are shown to the model as a one-line
    /// stub from here on.
    ///
    /// No longer written: [`SessionEvent::MessagesRewritten`] carries the text
    /// itself. Still projected, so a log that holds one replays as it ran.
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
    /// From here on the model sees `text` in place of what event `seq` said.
    ///
    /// The other half of compaction, and the half a fold cannot do: a long turn
    /// that no longer fits has nothing *settled* to fold away, and what fills the
    /// window is almost always tool output nobody needs in full any more — or a
    /// single message too large to send at all. The replacement is committed
    /// word for word rather than as a rule re-applied on replay, so a later
    /// change to how a stub is written never changes what an old log projects,
    /// and the rewrite is monotonic: the prefix cache is invalidated once.
    MessagesRewritten {
        turn: u64,
        texts: Vec<RewrittenText>,
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
        pause: crate::events::RateLimitPause,
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
    /// A hard boundary ended the turn and the person has to choose how to go on.
    ///
    /// Screen-visible, so logged: the recovery choices ("complete it yourself",
    /// "skip this step") are what a driver draws, and a session resumed after the
    /// process died owes the person the same choice rather than a silent stop.
    /// Not model-visible — the model already read the refusal as the tool's result.
    PolicyIntervention {
        turn: u64,
        intervention: atomcode_kernel::event::PolicyIntervention,
    },
    TurnEnd {
        turn: u64,
        /// Why it ended. The reason itself, not a rendering of it: a consumer
        /// that has to match on `"Cancelled"` to tell a stop from a failure is
        /// one typo away from calling a failed turn a clean one.
        stop: crate::seams::StopReason,
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
            | Self::MessagesRewritten { turn, .. }
            | Self::Usage { turn, .. }
            | Self::Notice { turn, .. }
            | Self::Titled { turn, .. }
            | Self::PolicyIntervention { turn, .. }
            | Self::Interrupted { turn, .. }
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
                | Self::MessagesRewritten { .. }
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
///
/// **5** — added [`SessionEvent::MessagesRewritten`], `from` on
/// [`SessionEvent::Compacted`], and [`NoticeKind::Compacting`] /
/// [`NoticeKind::CompactionDegraded`]: pressure-driven compaction folds tool
/// output in place and keeps a session's first request, which a fold alone
/// could not say.
pub const SESSION_FORMAT_VERSION: u32 = 5;

/// One replacement a [`SessionEvent::MessagesRewritten`] makes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewrittenText {
    /// The event whose text the model sees replaced.
    pub seq: SeqNo,
    pub text: String,
}

fn is_zero(seq: &SeqNo) -> bool {
    *seq == 0
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoggedEvent {
    pub seq: SeqNo,
    pub event: SessionEvent,
}

/// A committed fact, broadcast with the session it belongs to.
///
/// The id is not decoration. One process runs more than one log — a delegated
/// child has its own — and every listener registered above them sees all of
/// them, because that is what one-way realm visibility means. Without the id on
/// the broadcast, a subagent's transcript arrives on the parent's screen and in
/// the parent's file, and nothing downstream can tell it apart.
#[derive(Clone, Debug, PartialEq)]
pub struct Committed {
    /// Which log this fact was appended to.
    pub session: String,
    pub seq: SeqNo,
    pub event: SessionEvent,
}

impl Committed {
    /// The log record, without the routing information.
    pub fn logged(&self) -> LoggedEvent {
        LoggedEvent {
            seq: self.seq,
            event: self.event.clone(),
        }
    }
}

/// The append-only log.
///
/// The only writer is whoever holds the service; readers project. Appending
/// returns the assigned [`SeqNo`] so a caller can reference the exact fact it
/// just recorded (a compaction boundary, a persistence cursor).
pub struct SessionLog {
    header: SessionHeader,
    events: RwLock<Vec<LoggedEvent>>,
    next_seq: AtomicU64,
    turn: AtomicU64,
}

impl SessionLog {
    pub fn new(id: impl Into<String>) -> Self {
        Self::with_header(SessionHeader::new(id))
    }

    pub fn with_header(header: SessionHeader) -> Self {
        Self {
            header,
            events: RwLock::new(Vec::new()),
            next_seq: AtomicU64::new(1),
            turn: AtomicU64::new(0),
        }
    }

    pub fn id(&self) -> &str {
        &self.header.id
    }

    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// The session's name: the last `Titled` fact among its *own* events. A
    /// title inherited from a parent is the parent's, so a fork has none until
    /// it is named.
    pub fn title(&self) -> Option<String> {
        self.events
            .read()
            .expect("session log poisoned")
            .iter()
            .skip(self.header.inherited)
            .rev()
            .find_map(|e| match &e.event {
                SessionEvent::Titled { title, .. } => Some(title.clone()),
                _ => None,
            })
    }

    /// Claim the next turn number.
    ///
    /// Deliberately does *not* append `TurnStart`. Appending directly bypasses
    /// whoever broadcasts committed events, so the boundary would exist in
    /// memory and be missing from every consumer that learns by listening —
    /// persistence, projections, a UI. The caller commits it through the same
    /// path as every other fact.
    pub fn next_turn(&self) -> u64 {
        self.turn.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn current_turn(&self) -> u64 {
        self.turn.load(Ordering::SeqCst)
    }

    /// Append without telling anyone.
    ///
    /// Almost always the wrong call. A fact that is appended and not broadcast
    /// exists in memory and reaches nothing that learns by listening —
    /// persistence, projections, a UI — so it is silently missing from a
    /// resumed session. Use [`commit`](crate::session::commit) unless you are
    /// restoring a log that was already broadcast once.
    pub fn append(&self, event: SessionEvent) -> SeqNo {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        self.events
            .write()
            .expect("session log poisoned")
            .push(LoggedEvent { seq, event });
        seq
    }

    pub fn events(&self) -> Vec<LoggedEvent> {
        self.events.read().expect("session log poisoned").clone()
    }

    /// Events after `cursor`, for an incremental consumer (persistence, a UI
    /// that already rendered a prefix).
    pub fn since(&self, cursor: SeqNo) -> Vec<LoggedEvent> {
        self.events
            .read()
            .expect("session log poisoned")
            .iter()
            .filter(|e| e.seq > cursor)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.events.read().expect("session log poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Rebuild the log from persisted events (resume). Sequence numbers and the
    /// turn counter are restored from the data, not re-minted, so references
    /// taken before the restart stay valid.
    pub fn restore(&self, events: Vec<LoggedEvent>) {
        let max_seq = events.iter().map(|e| e.seq).max().unwrap_or(0);
        let max_turn = events.iter().map(|e| e.event.turn()).max().unwrap_or(0);
        *self.events.write().expect("session log poisoned") = events;
        self.next_seq.store(max_seq + 1, Ordering::SeqCst);
        self.turn.store(max_turn, Ordering::SeqCst);
    }

    /// Project the model's view of the conversation.
    ///
    /// This is the single place the log becomes a prompt. A consumer that needs
    /// "what the model saw" calls this rather than keeping a parallel list.
    pub fn derive_messages(&self) -> Vec<Message> {
        derive_messages(&self.events())
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
                SessionEvent::Compacted { through, from, .. } => {
                    *through = map(*through);
                    if *from != 0 {
                        *from = map(*from);
                    }
                }
                SessionEvent::ToolResultsStubbed { through, .. } => *through = map(*through),
                SessionEvent::MessagesRewritten { texts, .. } => {
                    for rewritten in texts {
                        rewritten.seq = map(rewritten.seq);
                    }
                }
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
        .into_iter()
        .map(|traced| traced.message)
        .collect()
}

/// What a projected message was made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// The text of the event at this sequence number: the text a
    /// [`SessionEvent::MessagesRewritten`] replaces.
    Event(SeqNo),
    /// A compaction summary: a `Compacted` event's, or one a resumed session was
    /// seeded with.
    Summary(SeqNo),
    /// Written beside the event at this sequence number rather than taken from
    /// it — a cancelled call's result, an interruption, a picture's carrier.
    Derived(SeqNo),
}

impl Provenance {
    pub fn seq(&self) -> SeqNo {
        match self {
            Self::Event(seq) | Self::Summary(seq) | Self::Derived(seq) => *seq,
        }
    }
}

/// One model-visible message, and what it was made from.
#[derive(Clone, Debug)]
pub struct TracedMessage {
    pub message: Message,
    pub source: Provenance,
}

/// [`derive_messages`], with each message traced to the event it came from.
///
/// For a compaction policy that measures and cuts the conversation as the model
/// sees it, and has to say where it cut in terms of the log.
pub fn derive_traced(events: &[LoggedEvent]) -> Vec<TracedMessage> {
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
        .into_iter()
        .map(|traced| traced.message)
        .collect()
}

fn project(events: &[LoggedEvent], with_meta: bool) -> Vec<TracedMessage> {
    // Each compaction folds what lies between the head it keeps and its
    // boundary, and what one folded stays folded. Collect them first: replaying
    // then discarding would be wasted work and, worse, would let a dropped tool
    // result pair with a surviving call. The last one's summary is the one shown.
    let mut folded: Vec<(SeqNo, SeqNo)> = Vec::new();
    let mut summary: Option<(SeqNo, &str)> = None;
    // How far the stubbing has reached, for the same reason: a result is shown
    // stubbed because a later fact says so.
    let mut stubbed_through: SeqNo = 0;
    // What a later fact says the model sees instead. The last word wins.
    let mut rewritten: std::collections::HashMap<SeqNo, &str> = std::collections::HashMap::new();
    for logged in events {
        match &logged.event {
            SessionEvent::Compacted {
                through,
                summary: s,
                from,
                ..
            } => {
                folded.push((*from, *through));
                summary = Some((logged.seq, s));
            }
            SessionEvent::ToolResultsStubbed { through, .. } => {
                stubbed_through = (*through).max(stubbed_through);
            }
            SessionEvent::MessagesRewritten { texts, .. } => {
                for replaced in texts {
                    rewritten.insert(replaced.seq, &replaced.text);
                }
            }
            _ => {}
        }
    }
    let compacted_at = summary.map(|(seq, _)| seq).unwrap_or(0);
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
    let mut push = |message: Message, source: Provenance| {
        messages.push(TracedMessage { message, source });
    };
    if let Some((seq, summary)) = summary {
        let mut message = Message::system(summary);
        message.synthetic = true;
        push(message, Provenance::Summary(seq));
    }

    for logged in events.iter().filter(|e| {
        !folded
            .iter()
            .any(|(from, through)| e.seq > *from && e.seq <= *through)
    }) {
        let seq = logged.seq;
        let text_of = |own: &str| rewritten.get(&seq).copied().unwrap_or(own).to_string();
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
                                    push(
                                        Message::tool_result(&call.id, "(cancelled)", true),
                                        Provenance::Derived(seq),
                                    );
                                }
                            }
                        }
                    }
                }
                push(Message::user_interruption(), Provenance::Derived(seq));
            }
            SessionEvent::UserMessage { text, images, .. } => {
                let text = text_of(text);
                let message = if images.is_empty() {
                    Message::user(text)
                } else {
                    Message::user_with_images(text, images.clone())
                };
                push(message, Provenance::Event(seq));
            }
            // A summary a resumed session was seeded with stands for what came
            // before it — until a later compaction's summary stands for that too.
            SessionEvent::Injected {
                origin: InjectionOrigin::CompactionSummary,
                ..
            } if seq < compacted_at => {}
            SessionEvent::Injected { text, origin, .. } => {
                let mut message = match origin {
                    // A continuation speaks as the user, because it is a
                    // prompt; the rest ride as a note the model reads in place.
                    // A nudge is a prompt too — the harness asking for a check it
                    // is owed; what differs is only whether a talking-only answer
                    // is shown (see `crate::agent::MessageOrigin::Internal`).
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
                let source = match origin {
                    InjectionOrigin::CompactionSummary => Provenance::Summary(seq),
                    _ => Provenance::Derived(seq),
                };
                push(message, source);
            }
            SessionEvent::AssistantMessage {
                text,
                reasoning,
                tool_calls,
                reasoning_blocks,
                meta,
                ..
            } => {
                let mut message = Message::assistant(text_of(text), tool_calls.clone());
                if !reasoning.is_empty() {
                    message.reasoning = Some(reasoning.clone());
                }
                message.reasoning_blocks = reasoning_blocks.clone();
                if with_meta {
                    message.meta = meta.clone();
                }
                push(message, Provenance::Event(seq));
            }
            SessionEvent::ToolResultLogged {
                call_id,
                content,
                is_error,
                images,
                ..
            } => {
                let shown = if let Some(text) = rewritten.get(&seq) {
                    (*text).to_string()
                } else if seq <= stubbed_through {
                    atomcode_capabilities::compaction::build_compact_stub(
                        tool_names.get(call_id.as_str()).copied().unwrap_or("tool"),
                        content,
                        !*is_error,
                    )
                } else {
                    content.clone()
                };
                push(
                    Message::tool_result(call_id, &shown, *is_error),
                    Provenance::Event(seq),
                );
                // A provider serializes images on a user message and rejects
                // them on a tool one, so the picture rides in immediately
                // after the result it belongs to.
                if !images.is_empty() {
                    let mut carrier = Message::user_with_images("", images.clone());
                    carrier.synthetic = true;
                    push(carrier, Provenance::Derived(seq));
                }
            }
            // Chunks, headers, usage and turn boundaries are facts about the
            // session, not content the model receives.
            _ => {}
        }
    }
    messages
}

/// The runtime invariant: every model-visible message can be traced to a logged
/// event. Returns the offending messages rather than panicking, so a caller can
/// decide whether to fail the turn or report.
///
/// This is the check that keeps a side channel from quietly growing: if a plugin
/// starts appending straight to the request, this stops agreeing.
pub fn assert_model_visible_is_logged(
    log: &SessionLog,
    sent: &[Message],
) -> Result<(), Vec<String>> {
    let derived = log.derive_messages();
    let mut unexplained = Vec::new();
    for message in sent {
        // The system prompt is assembled, not logged: it is a pure function of
        // the mounted plugins and is reconstructible from the config tree.
        if message.role == atomcode_kernel::message::Role::System && !message.synthetic {
            continue;
        }
        if !derived
            .iter()
            .any(|d| d.role == message.role && d.text == message.text)
        {
            unexplained.push(format!(
                "{:?}: {}",
                message.role,
                truncate(&message.text, 80)
            ));
        }
    }
    if unexplained.is_empty() {
        Ok(())
    } else {
        Err(unexplained)
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// Commit one fact: append it, broadcast it, advance the projections.
///
/// The single write path, and the reason it is a free function rather than a
/// method on the log: the log cannot broadcast (it has no context), and every
/// caller that reached for `append` instead ended up with a fact that survived
/// in memory and vanished on resume. Turn boundaries, memory injections,
/// compaction cuts and truncation nudges were each lost that way.
pub fn commit(ctx: &atomcode_plexus::Context, log: &SessionLog, event: SessionEvent) -> SeqNo {
    let seq = log.append(event.clone());
    ctx.emit::<crate::events::SessionEventCommitted>(&Committed {
        session: log.id().to_string(),
        seq,
        event,
    });
    if let Some(projections) = ctx.service::<crate::seams::SessionProjectionsSvc>() {
        projections.advance(log);
    }
    seq
}

/// Apply a compaction decision: record what the model sees in other words, cut
/// the history at `through`, and tell the person anything they should know.
///
/// Four callers arrive at a decision by different routes — a usage threshold,
/// an overflow retry, an `AgentHandle` request, a `/compact` command — and each
/// has to turn it into the same facts. The
/// [`Compaction`](crate::seams::Compaction) seam deliberately cannot do it for
/// them: it takes a log and no context, so a policy stays a pure decision that
/// can be tested off the tree. This is where that one translation lives, so
/// there is no fifth spelling of it.
///
/// The rewrites land before the cut: a consumer that reacts to the cut (the
/// native store) then sees both.
pub fn apply_compaction(
    ctx: &atomcode_plexus::Context,
    log: &SessionLog,
    decision: crate::seams::CompactionDecision,
) -> SeqNo {
    let turn = log.current_turn();
    let mut last = 0;
    if !decision.rewrites.is_empty() {
        last = commit(
            ctx,
            log,
            SessionEvent::MessagesRewritten {
                turn,
                texts: decision.rewrites,
            },
        );
    }
    if decision.through != 0 {
        last = commit(
            ctx,
            log,
            SessionEvent::Compacted {
                turn,
                through: decision.through,
                summary: decision.summary,
                from: decision.from,
            },
        );
    }
    if let Some(note) = decision.note {
        commit(
            ctx,
            log,
            SessionEvent::Notice {
                turn,
                notice: NoticeKind::CompactionDegraded,
                detail: note,
            },
        );
    }
    last
}

// ---- projections --------------------------------------------------------

/// A unit that folds committed events into one typed state.
///
/// Registered by whoever owns the state, read by anyone through
/// [`SessionProjections::state_of`]. Consumers get a shared, incrementally
/// maintained view instead of each re-scanning the log.
pub trait ProjectionUnit: Send + Sync {
    /// Stable key consumers read by.
    fn key(&self) -> &'static str;
    /// Fold one event into the state. Must be pure and order-dependent only on
    /// the log, so a replay produces the same state.
    fn fold(&self, state: &mut serde_json::Value, event: &SessionEvent);
}

/// The projection registry.
#[derive(Default)]
pub struct SessionProjections {
    units: RwLock<Vec<Arc<dyn ProjectionUnit>>>,
    states: RwLock<BTreeMap<&'static str, serde_json::Value>>,
    cursor: AtomicU64,
}

impl SessionProjections {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, unit: Arc<dyn ProjectionUnit>) {
        let key = unit.key();
        self.states
            .write()
            .expect("projections poisoned")
            .entry(key)
            .or_insert(serde_json::Value::Null);
        self.units.write().expect("projections poisoned").push(unit);
    }

    pub fn unregister(&self, key: &str) {
        self.units
            .write()
            .expect("projections poisoned")
            .retain(|u| u.key() != key);
        self.states
            .write()
            .expect("projections poisoned")
            .retain(|k, _| *k != key);
    }

    /// Fold everything the registry has not seen yet.
    pub fn advance(&self, log: &SessionLog) {
        let cursor = self.cursor.load(Ordering::SeqCst);
        let pending = log.since(cursor);
        if pending.is_empty() {
            return;
        }
        let units = self.units.read().expect("projections poisoned").clone();
        let mut states = self.states.write().expect("projections poisoned");
        for logged in &pending {
            for unit in &units {
                let state = states.entry(unit.key()).or_insert(serde_json::Value::Null);
                unit.fold(state, &logged.event);
            }
        }
        self.cursor.store(
            pending.last().map(|e| e.seq).unwrap_or(cursor),
            Ordering::SeqCst,
        );
    }

    pub fn state_of(&self, key: &str) -> Option<serde_json::Value> {
        self.states
            .read()
            .expect("projections poisoned")
            .get(key)
            .cloned()
    }

    /// Every projected state, for a client that renders the whole session view.
    pub fn snapshot(&self) -> BTreeMap<String, serde_json::Value> {
        self.states
            .read()
            .expect("projections poisoned")
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    pub fn keys(&self) -> Vec<&'static str> {
        self.states
            .read()
            .expect("projections poisoned")
            .keys()
            .copied()
            .collect()
    }
}
