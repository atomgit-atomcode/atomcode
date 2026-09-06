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

use atomcode_kernel::message::{ImageContent, Message};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionOrigin {
    /// A persistent memory store.
    Memory,
    /// A `<system-reminder>` style runtime note.
    Reminder,
    /// A continuation the harness itself asked for.
    Continuation,
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
            | Self::ToolResultLogged { turn, .. }
            | Self::Injected { turn, .. }
            | Self::Compacted { turn, .. }
            | Self::Usage { turn, .. }
            | Self::Notice { turn, .. }
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
        )
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
    id: String,
    events: RwLock<Vec<LoggedEvent>>,
    next_seq: AtomicU64,
    turn: AtomicU64,
}

impl SessionLog {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            events: RwLock::new(Vec::new()),
            next_seq: AtomicU64::new(1),
            turn: AtomicU64::new(0),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
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

/// The projection, as a free function so it can be tested against a literal log
/// and reused by a persistence layer replaying someone else's events.
pub fn derive_messages(events: &[LoggedEvent]) -> Vec<Message> {
    // A compaction boundary replaces everything at or below it. Find the last
    // one first: replaying then discarding would be wasted work and, worse,
    // would let a dropped tool result pair with a surviving call.
    let mut floor: SeqNo = 0;
    let mut summary: Option<&str> = None;
    for logged in events {
        if let SessionEvent::Compacted {
            through,
            summary: s,
            ..
        } = &logged.event
        {
            floor = *through;
            summary = Some(s);
        }
    }

    let mut messages = Vec::new();
    if let Some(summary) = summary {
        let mut message = Message::system(summary);
        message.synthetic = true;
        messages.push(message);
    }

    for logged in events.iter().filter(|e| e.seq > floor) {
        match &logged.event {
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
                    // prompt; the rest are context, which is a system note.
                    InjectionOrigin::Continuation => Message::user(text),
                    _ => Message::system(text),
                };
                message.synthetic = true;
                messages.push(message);
            }
            SessionEvent::AssistantMessage {
                text,
                reasoning,
                tool_calls,
                ..
            } => {
                let mut message = Message::assistant(text, tool_calls.clone());
                if !reasoning.is_empty() {
                    message.reasoning = Some(reasoning.clone());
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
                messages.push(Message::tool_result(call_id, content, *is_error));
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

/// Apply a compaction decision: cut the history at `through`, and record what
/// the model sees in place of what was cut.
///
/// Four callers arrive at a decision by different routes — a usage threshold,
/// an overflow retry, an `AgentHandle` request, a `/compact` command — and each
/// has to turn it into the same fact. The
/// [`Compaction`](crate::seams::Compaction) seam deliberately cannot do it for
/// them: it takes a log and no context, so a policy stays a pure decision that
/// can be tested off the tree. This is where that one translation lives, so
/// there is no fifth spelling of it.
pub fn apply_compaction(
    ctx: &atomcode_plexus::Context,
    log: &SessionLog,
    decision: crate::seams::CompactionDecision,
) -> SeqNo {
    commit(
        ctx,
        log,
        SessionEvent::Compacted {
            turn: log.current_turn(),
            through: decision.through,
            summary: decision.summary,
        },
    )
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
