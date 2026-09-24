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
use crate::message::{ImageContent, Message, MessageMeta, ReasoningBlock, Role};
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
    /// What the person said directly to one of this agent's team members
    /// (`docs/adr/0023` §7): the lead is told, not asked. `member` is its name.
    PersonToMember { member: String },
    /// Something about a team member the lead should know and need not act on
    /// now — its report on a turn the person started, its being stopped with
    /// nothing of the lead's outstanding.
    TeamNote { member: String },
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
        /// The PERSON named it (via `/rename` / `/title`), rather than the runtime
        /// auto-guessing from the first prompt. Drivers show a user-chosen name
        /// more prominently (e.g. a pill on the composer). `#[serde(default)]` so a
        /// v1 log without the field reads as an auto title.
        #[serde(default)]
        user_set: bool,
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
    /// What a reply had streamed when the person stopped it: the text and the
    /// reasoning received for a step that never became an
    /// [`SessionEvent::AssistantMessage`] (`docs/adr/0024` §8–9).
    ///
    /// Chunks are not kept on disk, so without this the part of the answer the
    /// person had already read would be gone from a resumed session — and from
    /// the model, which is told it was interrupted without being told where.
    /// Committed just before [`SessionEvent::Interrupted`], only for a person's
    /// cancel and only when something had arrived. A kept turn shows the model
    /// its text as an assistant message ahead of the interruption note; the
    /// reasoning stays in the log for a screen, never in a request (an unsigned
    /// thinking block is one some providers refuse), and a tool call that had
    /// not finished is not here at all — it would be a call with no result.
    PartialReply {
        turn: u64,
        round: u32,
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning: String,
    },
    /// The agent was stopped for good and taken off its team
    /// (`docs/adr/0024` §13). Its log stays readable; a resume of the lead does
    /// not bring it back.
    Stopped {
        turn: u64,
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
    /// The workspace was checkpointed as `turn` started: `id` names what a
    /// rewind of the workspace to before that turn restores (`docs/adr/0024`
    /// §17). Not model-visible.
    Checkpointed {
        turn: u64,
        id: String,
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
            | Self::PartialReply { turn, .. }
            | Self::Stopped { turn }
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
            | Self::Rewound { turn, .. }
            | Self::Checkpointed { turn, .. }
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
                | Self::PartialReply { .. }
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
/// **5** — added [`SessionEvent::Rewound`] and [`RewindScope`]: an undo, a
/// rewind or a restore is a fact the projection honours, not a rewritten log
/// (`docs/adr/0024` §17).
///
/// **6** — added [`SessionEvent::PartialReply`]: what a reply had said when the
/// person stopped it, now that chunks are not kept (`docs/adr/0024` §7–9).
///
/// **7** — added [`SessionEvent::Stopped`]: a team member stopped for good,
/// which a resume of its lead leaves where it is (`docs/adr/0024` §13).
///
/// **8** — added [`InjectionOrigin::PersonToMember`] and
/// [`InjectionOrigin::TeamNote`]: what a lead is told about its team without
/// being woken (`docs/adr/0023` §7).
///
/// **9** — brought in what the compaction line added beside this one, which it
/// had numbered 5: [`SessionEvent::MessagesRewritten`], `from` on
/// [`SessionEvent::Compacted`], and [`NoticeKind::Compacting`] /
/// [`NoticeKind::CompactionDegraded`] — pressure-driven compaction folds tool
/// output in place and keeps a session's first request, which a fold alone
/// could not say.
///
/// **10** — added [`SessionEvent::Checkpointed`]: which workspace checkpoint a
/// turn started from (`docs/adr/0024` §17). It was written as 9 while the
/// compaction line was on its own branch; both were 9, and a file is only ever
/// read against one of these numbers, so the later one to land moves.
///
/// A file's header records the version that created it; a later build may
/// append facts of a kind added since. A reader that meets a kind it does not
/// know treats the file as newer than itself, the same refusal.
pub const SESSION_FORMAT_VERSION: u32 = 10;

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
    /// For a team member: what it was created with (`docs/adr/0024` §13). Set
    /// once at creation, like the rest of the header; a resume recreates the
    /// member from it, with its tools and permissions worked out again from the
    /// role as it is defined then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<MemberHeader>,
}

/// What a team member was created with.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MemberHeader {
    pub name: String,
    /// The role's id; its definition is looked up again on a resume.
    pub role: String,
    /// The task it was first given.
    pub task: String,
    /// The model it was put on by name, if one was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The thinking level it was given, if one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Its own checkout, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The files it may write, when it shares the lead's workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
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
            member: None,
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

/// What the compactions that stand fold away: each one's `(from, through]`.
/// One that was taken back no longer folds anything.
fn folded_ranges(
    events: &[LoggedEvent],
    taken_back: &impl Fn(SeqNo) -> bool,
) -> Vec<(SeqNo, SeqNo)> {
    events
        .iter()
        .filter(|logged| !taken_back(logged.seq))
        .filter_map(|logged| match logged.event {
            SessionEvent::Compacted { through, from, .. } => Some((from, through)),
            _ => None,
        })
        .collect()
}

/// Which turns the conversation still shows: opened after the compaction that
/// stands, not taken back by an undo, not undone after an interruption.
///
/// What is kept beside a log per turn — statistics, display entries — follows
/// this rather than counting messages, so it agrees with [`derive_messages`]
/// about which turns are gone.
pub fn visible_turns(events: &[LoggedEvent]) -> std::collections::BTreeSet<u64> {
    let taken_back = taken_back(events);
    let folded = folded_ranges(events, &taken_back);
    let undone: std::collections::HashSet<u64> = events
        .iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::Interrupted { turn, undone: true } => Some(turn),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter(|logged| {
            !taken_back(logged.seq)
                && !folded
                    .iter()
                    .any(|(from, through)| logged.seq > *from && logged.seq <= *through)
        })
        .filter_map(|logged| match logged.event {
            SessionEvent::TurnStart { turn } if !undone.contains(&turn) => Some(turn),
            _ => None,
        })
        .collect()
}

/// Which turns were taken back — by an undo, a rewind of the conversation, or a
/// person's interruption they asked to have undone. A compacted turn is not
/// among them: it was summarised, not withdrawn.
pub fn undone_turns(events: &[LoggedEvent]) -> std::collections::BTreeSet<u64> {
    let taken_back = taken_back(events);
    events
        .iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::TurnStart { turn } if taken_back(logged.seq) => Some(turn),
            SessionEvent::Interrupted { turn, undone: true } => Some(turn),
            _ => None,
        })
        .collect()
}

/// Which turns a **rewind** took back — the subset of [`undone_turns`] a person
/// asked for by name, leaving out the ones a cancel withdrew.
///
/// The two are drawn differently, which is why they are counted separately: a
/// turn a person rewound past is one they said should not have happened, and
/// the screen takes it off (`docs/adr/0024` §17). A turn they *cancelled* is
/// one they stopped halfway, and what it got done before they stopped is still
/// worth reading — so that one stays, dimmed.
pub fn rewound_turns(events: &[LoggedEvent]) -> std::collections::BTreeSet<u64> {
    let taken_back = taken_back(events);
    events
        .iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::TurnStart { turn } if taken_back(logged.seq) => Some(turn),
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

/// 投影的落笔处：把一条条事实落成模型可见的消息。
///
/// 它存在的唯一理由是图片。provider 只在 user 消息上序列化图片（tool 消息上带图直接
/// 400），所以工具带回来的图必须另起一条 user 消息承载；而那条消息**不能**插在同一批
/// tool 结果中间——assistant 的 `tool_calls` 后面必须紧跟这批结果的全部 tool 消息，
/// 中间夹一条 user 会让整个请求被拒（`insufficient tool messages following tool_calls
/// message`）。一批里每来一条带图的结果就落一条承载消息正是这个错误：两次 `read_file`
/// 读图就够把会话锁死，而每轮请求都由这份投影重建，于是每个「继续」都原样再失败一次。
///
/// 所以图片先攒着（[`take_images`](Self::take_images)），等这条流水线上落下第一条
/// **不是** tool 结果的消息时（或日志走完时）才作为一条 user 承载消息落下，紧跟这一批
/// 结果之后。这与回合引擎 live 路径的时机一致：`agent/engine.rs` 把一批图片攒进
/// `turn_images`，批结束后才落一条。
struct Projection {
    messages: Vec<TracedMessage>,
    /// 已收下、还没落成承载消息的图片。
    pending: Vec<ImageContent>,
    /// 第一条带图结果的序号，给承载消息做 provenance。
    pending_from: SeqNo,
}

impl Projection {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            pending: Vec::new(),
            pending_from: 0,
        }
    }

    /// 落一条消息。落下之前，若这一条不是 tool 结果而手里还攒着图片，说明上一批结果
    /// 已经走完：先把承载消息落了，图片才不会挤进这批结果中间。
    fn push(&mut self, message: Message, source: Provenance) {
        if message.role != Role::Tool {
            self.flush_images();
        }
        self.messages.push(TracedMessage { message, source });
    }

    /// 收下一条工具结果带的图片，**不**当场落消息（理由见 struct 的说明）。
    fn take_images(&mut self, from: SeqNo, images: &[ImageContent]) {
        if images.is_empty() {
            return;
        }
        if self.pending.is_empty() {
            self.pending_from = from;
        }
        self.pending.extend(images.iter().cloned());
    }

    /// 日志走完：攒着的图片也要落下。
    fn finish(mut self) -> Vec<TracedMessage> {
        self.flush_images();
        self.messages
    }

    fn flush_images(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut carrier = Message::user_with_images("", std::mem::take(&mut self.pending));
        carrier.synthetic = true;
        self.messages.push(TracedMessage {
            message: carrier,
            source: Provenance::Derived(self.pending_from),
        });
    }
}

fn project(events: &[LoggedEvent], with_meta: bool) -> Vec<TracedMessage> {
    let taken_back = taken_back(events);

    // Each compaction folds what lies between the head it keeps and its
    // boundary, and what one folded stays folded. Collect them first: replaying
    // then discarding would be wasted work and, worse, would let a dropped tool
    // result pair with a surviving call. The last one's summary is the one shown.
    // One that was taken back no longer counts, and the ones before it hold.
    let folded = folded_ranges(events, &taken_back);
    let mut summary: Option<(SeqNo, &str)> = None;
    // How far the stubbing has reached, for the same reason: a result is shown
    // stubbed because a later fact says so.
    let mut stubbed_through: SeqNo = 0;
    // What a later fact says the model sees instead. The last word wins.
    let mut rewritten: std::collections::HashMap<SeqNo, &str> = std::collections::HashMap::new();
    for logged in events.iter().filter(|logged| !taken_back(logged.seq)) {
        match &logged.event {
            SessionEvent::Compacted { summary: s, .. } => {
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

    let mut projection = Projection::new();
    if let Some((seq, summary)) = summary {
        let mut message = Message::system(summary);
        message.synthetic = true;
        projection.push(message, Provenance::Summary(seq));
    }

    for logged in events.iter().filter(|e| {
        !folded
            .iter()
            .any(|(from, through)| e.seq > *from && e.seq <= *through)
    }) {
        let seq = logged.seq;
        let text_of = |own: &str| rewritten.get(&seq).copied().unwrap_or(own).to_string();
        // What the lead is told about its team is not the work of whichever
        // of its turns it landed in either, so undoing that turn keeps it.
        let session_wide = matches!(
            &logged.event,
            SessionEvent::Injected {
                origin: InjectionOrigin::Memory
                    | InjectionOrigin::CompactionSummary
                    | InjectionOrigin::PersonToMember { .. }
                    | InjectionOrigin::TeamNote { .. },
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
                                    projection.push(
                                        Message::tool_result(&call.id, "(cancelled)", true),
                                        Provenance::Derived(seq),
                                    );
                                }
                            }
                        }
                    }
                }
                projection.push(Message::user_interruption(), Provenance::Derived(seq));
            }
            SessionEvent::UserMessage { text, images, .. } => {
                let text = text_of(text);
                let message = if images.is_empty() {
                    Message::user(text)
                } else {
                    Message::user_with_images(text, images.clone())
                };
                projection.push(message, Provenance::Event(seq));
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
                    // The person's own words, but to a member rather than to
                    // this agent: something the member now acts on, which the
                    // lead coordinates around rather than overrides.
                    InjectionOrigin::PersonToMember { member } => Message::user(format!(
                        "[the person said this directly to your team member `{member}`, which \
                         is acting on it — a correction or an addition, not an instruction to \
                         you; do not override it when you coordinate]\n{text}"
                    )),
                    InjectionOrigin::TeamNote { member } => Message::user(format!(
                        "[about your team member `{member}` — for your information; not the \
                         user, and nothing you are asked to do now]\n{text}"
                    )),
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
                projection.push(message, source);
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
                projection.push(message, Provenance::Event(seq));
            }
            SessionEvent::PartialReply { text, .. } if !text.is_empty() => {
                projection.push(
                    Message::assistant(text_of(text), Vec::new()),
                    Provenance::Event(seq),
                );
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
                    build_compact_stub(
                        tool_names.get(call_id.as_str()).copied().unwrap_or("tool"),
                        content,
                        !*is_error,
                    )
                } else {
                    content.clone()
                };
                projection.push(
                    Message::tool_result(call_id, &shown, *is_error),
                    Provenance::Event(seq),
                );
                // A provider serializes images on a user message and rejects
                // them on a tool one, so the picture rides in a carrier user
                // message of its own — but only once this batch's results have
                // ALL landed: a user message between two tool results of one
                // assistant `tool_calls` is exactly the payload a provider
                // rejects as "insufficient tool messages following tool_calls
                // message". Two image results in one batch used to break the
                // session permanently, since every request is rebuilt from this
                // projection. `Projection` holds them until the run of tool
                // messages ends (or the log does).
                projection.take_images(seq, images);
            }
            // Chunks, headers, usage and turn boundaries are facts about the
            // session, not content the model receives.
            _ => {}
        }
    }
    projection.finish()
}

// ---- questions put to a person ------------------------------------------

/// The three answers an approval can have. The spelling is
/// `atomcode_capabilities::tools::approval`'s, so a decision means the same
/// thing whichever gate asked and whatever carries it.
pub const ANSWER_ALLOW: &str = "allow";
pub const ANSWER_ALWAYS: &str = "allow_always";
/// Allow AND remember for ALL calls of this call's group (e.g. every non-sensitive
/// `bash`) this session — the blanket "本会话允许所有 Bash". Offered only when the
/// call is eligible (see [`crate::tool::Tool::allow_all_group`]).
pub const ANSWER_ALWAYS_ALL: &str = "allow_always_all";
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
        allow_all: Option<&str>,
    ) -> Self {
        let mut options = vec![Answer::labelled(ANSWER_ALLOW, "allow once")];
        if grant.is_some() {
            options.push(Answer::labelled(ANSWER_ALWAYS, "always allow"));
        }
        // The session-wide blanket for this call's group (`Some("bash")`), offered
        // only when the policy says it may be — a sensitive target keeps it `None`,
        // so the floor is not something a driver can accidentally offer past.
        if let Some(group) = allow_all {
            options.push(Answer::labelled(
                ANSWER_ALWAYS_ALL,
                format!("allow all {group} this session"),
            ));
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
