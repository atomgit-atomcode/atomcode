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

use atomcode_kernel::message::Message;

/// The vocabulary lives in the kernel (`docs/adr/0024` §6); re-exported whole so
/// every `crate::session::…` path keeps working.
pub use atomcode_kernel::session::*;

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
    /// What stamps a record with when it was committed.
    clock: Arc<dyn atomcode_kernel::clock::WallClock>,
}

impl SessionLog {
    pub fn new(id: impl Into<String>) -> Self {
        Self::with_header(SessionHeader::new(id))
    }

    pub fn with_header(header: SessionHeader) -> Self {
        Self::with_clock(header, Arc::new(atomcode_kernel::clock::SystemWallClock))
    }

    /// A log whose records are stamped by `clock`.
    pub fn with_clock(
        header: SessionHeader,
        clock: Arc<dyn atomcode_kernel::clock::WallClock>,
    ) -> Self {
        Self {
            header,
            events: RwLock::new(Vec::new()),
            next_seq: AtomicU64::new(1),
            turn: AtomicU64::new(0),
            clock,
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
        self.record(event).0
    }

    /// Append, and say both the sequence number and the commit time assigned.
    pub fn record(&self, event: SessionEvent) -> (SeqNo, u64) {
        let at = self.clock.now_ms();
        let mut events = self.events.write().expect("session log poisoned");
        // Under the lock, so the order of sequence numbers is the order of the
        // records.
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        events.push(LoggedEvent { seq, at, event });
        (seq, at)
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
    let (seq, at) = log.record(event.clone());
    ctx.emit::<crate::events::SessionEventCommitted>(&Committed {
        session: log.id().to_string(),
        seq,
        at,
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
