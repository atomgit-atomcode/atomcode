//! The agent as a first-class entity: an inbox, a lifecycle, and a realm.
//!
//! Before this, "the agent" was whatever the loop happened to be doing inside
//! one `run_turn` call. That is enough for a CLI that asks one question at a
//! time, and not enough for anything else: there is nowhere to deliver a second
//! message to, nothing to observe while work is in flight, and no handle to
//! cancel.
//!
//! # A turn is not a function call
//!
//! A **step** is one model request plus the tools it calls. A **turn** contains
//! zero or more steps: it opens before the first input is claimed and closes
//! when nothing is owed — no pending tool results, and nothing left in the inbox
//! that would wake it. That distinction is what lets a message arriving
//! mid-turn be folded into the turn already running instead of queueing behind
//! it, which is what steering is.
//!
//! # Injection rides, it does not drive
//!
//! Two kinds of things arrive in an inbox. A **message** wakes the agent: it is
//! someone asking for work. An **injection** is context — a memory, a reminder,
//! a note from a hook — and it must never start a turn on its own, or a
//! background reminder would keep an idle agent billing forever. It waits in
//! the inbox until a real message carries it in.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use atomcode_plexus::{Context, Disposable};
use tokio_util::sync::CancellationToken;

use atomcode_kernel::message::ImageContent;

use crate::events::{
    AgentChange, AgentCreated, AgentInfo, AgentRemoved, AgentStatusChanged, DescribeAgent,
    Describing, InboxInserted,
};
use crate::seams::{
    CompactionSvc, FsSvc, LlmSvc, SessionDefaultsSvc, SessionPersistenceSvc, SessionSvc,
};
use crate::session::{InjectionOrigin, LoggedEvent, SessionEvent, SessionHeader, SessionLog};

// ---- the agent whose turn this is -----------------------------------------

tokio::task_local! {
    /// The context of the agent whose turn is running on this task.
    static CURRENT: Context;
}

/// The agent whose turn this task is running, if any.
///
/// Set by the driver for the duration of a turn ([`as_agent`]) and inherited
/// by everything the turn awaits: waterfall listeners, tools, the compaction
/// and recovery policies. None of those hold an agent — they are registered
/// once, on a plugin's context at the top of the tree — yet each acts on behalf
/// of whichever agent's request it is handling. Before this, every one of them
/// resolved the log through its own context and therefore always found the
/// tree's, so a delegated child's compaction cut the parent's history.
///
/// deepseek-harness solves the same problem the same way (`AgentRegistry`'s
/// `initiators` AsyncLocalStorage); the Rust spelling is a task-local.
pub fn current() -> Option<Context> {
    CURRENT.try_with(|c| c.clone()).ok()
}

/// The context to resolve through: the running agent's when there is one,
/// otherwise `ctx`. What a listener registered at the top of the tree calls
/// instead of using its own context directly.
pub fn scoped(ctx: &Context) -> Context {
    current().unwrap_or_else(|| ctx.clone())
}

/// The name of the delegated agent whose turn this is, if it is one.
///
/// `None` means the conversation itself — the agent the person is driving.
/// A member is one that has a parent; its name is the last segment of its
/// session id, which is the name the lead gave it when it delegated.
///
/// Here rather than in the two rows that want it: the approval gate and
/// `ask_user` both put a question to a person, and "who is asking" has to read
/// the same way in both or the person learns to distrust it.
pub fn current_member_name(ctx: &Context) -> Option<String> {
    let current = current()?;
    let log = current.service::<crate::seams::SessionSvc>()?;
    let agent = ctx
        .service::<crate::seams::AgentsSvc>()?
        .by_session(log.id())?;
    agent.parent()?;
    let id = agent.session_id();
    Some(id.rsplit('/').next().unwrap_or(id).to_string())
}

/// The log of the conversation the person is driving.
///
/// [`current_member_name`]'s counterpart, and here for the same reason: a
/// question asked inside a member's turn is the member's turn, but it is the
/// person's conversation that has to be able to redraw it afterwards. A
/// member's own log is its parent's business — it is not even persisted (see
/// the session-persistence row) — so a card written there would be written
/// where nobody reads, and the screen would be back to showing something the
/// log cannot explain.
///
/// With no turn running on this task — a front end asking on the person's
/// behalf — the conversation is the root agent's, which is the same rule the
/// registry uses to mean "not delegated".
fn person_session(ctx: &Context) -> Option<Arc<SessionLog>> {
    let agents = ctx.service::<crate::seams::AgentsSvc>()?;
    if let Some(here) = current().and_then(|c| c.service::<SessionSvc>()) {
        return match agents
            .by_session(here.id())
            .and_then(|a| a.parent().map(str::to_string))
        {
            Some(parent) => agents.by_session(&parent).map(|a| a.session()),
            None => Some(here),
        };
    }
    agents
        .list()
        .into_iter()
        .find(|a| a.parent().is_none())
        .map(|a| a.session())
}

/// Write down that a question was put, before it is answered.
///
/// Public because there are two ways to ask and both have to reach the log: the
/// `user-questions` seam ([`ask_person`], which owns the whole exchange) and the
/// handle's own `approval` round-trip, which speaks the driver's wire contract
/// and calls this from the row that owns that request. A single funnel would
/// have meant changing that wire shape, and a session a client can rejoin is
/// not worth a new protocol for every front end.
pub fn record_asked(ctx: &Context, question: &crate::seams::Question) {
    let Some(log) = person_session(ctx) else {
        return;
    };
    crate::session::commit(
        &scoped(ctx),
        &log,
        SessionEvent::Asked {
            turn: log.current_turn(),
            question: question.clone(),
        },
    );
}

/// Write down the answer — or that there was none.
///
/// `answer` is a [`crate::seams::Answer::value`], not whatever a caller's own
/// vocabulary calls the decision: the log holds what the person chose, and the
/// policy that asked is free to read it however it likes.
pub fn record_answered(ctx: &Context, answer: Option<String>) {
    let Some(log) = person_session(ctx) else {
        return;
    };
    // The front end's own name for itself. Read here rather than passed in,
    // because it is a property of whoever filled the seam, and a caller that
    // had to remember to thread it through is one that will eventually forget.
    let by = ctx
        .service::<crate::seams::UserQuestionsSvc>()
        .map(|q| q.describe())
        .unwrap_or_else(|| "nobody".to_string());
    crate::session::commit(
        &scoped(ctx),
        &log,
        SessionEvent::Answered {
            turn: log.current_turn(),
            answer,
            by,
        },
    );
}

/// Ask the person, and write down both halves of the exchange.
///
/// The way through for every row that asks through `user-questions`, rather
/// than calling the seam directly: the asking row is the only one holding both
/// halves — the question as it was put and the answer that came back — and a
/// front end sees them as two unrelated things. What it writes is a fact, so a
/// panel can be remounted into an answered card and a resumed session can show
/// what was decided.
///
/// **Asked first, then the answer.** A question that is waiting is itself a
/// state a client has to be able to see: one that connects midway folds the log
/// and finds the question it can answer, instead of a session that looks
/// blocked for no reason. See [`SessionEvent::Asked`] and
/// [`SessionEvent::Answered`].
pub async fn ask_person(ctx: &Context, question: crate::seams::Question) -> Option<String> {
    let questions = ctx.service::<crate::seams::UserQuestionsSvc>()?;
    record_asked(ctx, &question);
    // Asked of the *service that will answer*, after the fact is down: a
    // front end that resolves the question by reading back the log would
    // otherwise be racing the write that put it there.
    let answer = questions.ask(&question).await;
    record_answered(ctx, answer.clone());
    answer
}

/// Run `f` as this agent's turn: everything it awaits resolves in the agent's
/// realm through [`scoped`].
pub async fn as_agent<F: std::future::Future>(ctx: Context, f: F) -> F::Output {
    CURRENT.scope(ctx, f).await
}

/// The log of the only agent in this tree.
///
/// For a caller that owns exactly one agent and has no handle on it — a test
/// after `App::start`, a front end that never delegates. Anything that can hold
/// the agent should ask it directly; with two agents this refuses rather than
/// guessing, because "the" session no longer names one thing.
pub trait OnlySession {
    fn only_session(&self) -> Option<Arc<SessionLog>>;
}

impl OnlySession for Context {
    fn only_session(&self) -> Option<Arc<SessionLog>> {
        if let Some(log) = current().and_then(|c| c.service::<SessionSvc>()) {
            return Some(log);
        }
        let agents = self.service::<crate::seams::AgentsSvc>()?;
        match agents.list().as_slice() {
            [only] => Some(only.session()),
            _ => None,
        }
    }
}

pub(crate) fn mint_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{now}-{}", std::process::id())
}

/// Creation-time composition of an agent's world: runs on the agent's realm
/// after the log is in place and before the agent is visible to anyone. The
/// registrations it returns are the agent's to tear down.
pub type Setup = Box<dyn FnOnce(&Context) -> Result<Vec<Disposable>, String> + Send>;

/// Everything an agent is given at birth.
///
/// The shape is deepseek-harness's `CreateAgentOptions`: the session identity
/// is the caller's to supply (an ACP client hands one over; a front end mints
/// one), the seed is how a fork or a resume begins with history, and `setup`
/// is where the caller mounts what only this agent should see.
#[derive(Default)]
pub struct CreateAgent {
    /// The session id. Minted when absent.
    pub id: Option<String>,
    /// The world's root. When set, this agent reads and writes under it
    /// regardless of what the tree's `fs` row was given.
    pub cwd: Option<PathBuf>,
    /// The session this one was forked from.
    pub parent: Option<String>,
    /// History to begin with. For a fork, a prefix of the parent's log; for a
    /// resume, this session's own stored log in full.
    pub seed: Vec<LoggedEvent>,
    /// How much of `seed` is inherited rather than this session's own work.
    /// Explicit because the two shapes above are indistinguishable by length:
    /// a fork inherits all of its seed, and so does a resume — but a resumed
    /// seed is the whole session, not a prefix of someone else's.
    pub seed_len: usize,
    /// Load `seed` from the persistence seam under `id` when none was given.
    pub resume: bool,
    /// Whether the persistence seam should keep this session.
    pub persist: bool,
    /// For a team member: what it was created with, into its header.
    pub member: Option<crate::session::MemberHeader>,
    pub setup: Option<Setup>,
}

impl CreateAgent {
    pub fn new() -> Self {
        Self {
            persist: true,
            ..Self::default()
        }
    }

    /// The front end's own agent: whatever the `session` row says about id and
    /// resume, so `--resume <id>` keeps meaning what it meant.
    pub fn root(ctx: &Context) -> Self {
        let defaults = ctx.service::<SessionDefaultsSvc>();
        let seed = defaults
            .as_ref()
            .map(|d| d.seed.clone())
            .unwrap_or_default();
        let seed_len = seed.len();
        Self {
            id: defaults.as_ref().and_then(|d| d.id.clone()),
            resume: defaults.map(|d| d.resume).unwrap_or(false),
            seed,
            seed_len,
            ..Self::new()
        }
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
    pub fn parent(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        self
    }
    pub fn seed(mut self, seed: Vec<LoggedEvent>, seed_len: usize) -> Self {
        self.seed = seed;
        self.seed_len = seed_len;
        self
    }
    pub fn resume(mut self, resume: bool) -> Self {
        self.resume = resume;
        self
    }
    pub fn persist(mut self, persist: bool) -> Self {
        self.persist = persist;
        self
    }
    pub fn member(mut self, member: crate::session::MemberHeader) -> Self {
        self.member = Some(member);
        self
    }
    pub fn setup(mut self, setup: Setup) -> Self {
        self.setup = Some(setup);
        self
    }
}

pub type AgentId = u64;

/// The contract's status, which is the agent's own: one definition, so what a
/// front end is told is what the agent is.
pub use atomcode_kernel::agent::{AgentDescription, AgentStatus};

/// Who asked for a turn.
///
/// A continuation the harness scheduled for itself is still someone asking for
/// work — it wakes an idle agent exactly like a typed message does. What it is
/// not is something the user said, and a transcript that cannot tell the two
/// apart shows the user saying things they never said.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MessageOrigin {
    /// A person, or whatever is standing in for one.
    #[default]
    User,
    /// The harness itself: a continuation, a scheduled goal, a resumed plan.
    Harness,
    /// The harness nudging the model to finish something it started — a check it
    /// owes, a list it left open. The nudge is logged like any other injection;
    /// what is different is the ANSWER: a reply that only talks (no tool call) is
    /// the model arguing with a note the person never wrote, and a driver does
    /// not show it. One that acts is shown by its actions, as usual.
    Internal,
    /// Another agent, by registry id. Logged with the sender's session id.
    Peer(AgentId),
}

/// One item waiting for an agent.
#[derive(Clone, Debug, PartialEq)]
pub enum InboxItem {
    /// Someone asking for work. Wakes an idle agent.
    Message {
        text: String,
        origin: MessageOrigin,
        /// Attachments that belong to this message. Model-visible, so they
        /// travel with it rather than being handed to the request separately.
        images: Vec<ImageContent>,
        /// The driver's id for the command that sent this, when it asked for a
        /// receipt. Never logged: it names a command, not a fact of the session.
        receipt: Option<atomcode_kernel::event::CommandId>,
    },
    /// Model-visible context that rides along with the next message. Never
    /// wakes anything on its own.
    Injection {
        text: String,
        origin: InjectionOrigin,
    },
}

/// What one step was given to work with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Claimed {
    /// The message this step is answering, if any.
    pub message: Option<String>,
    /// Who asked. Meaningless when there is no message.
    pub origin: MessageOrigin,
    /// What the message carried.
    pub images: Vec<ImageContent>,
    /// Context that came along with it.
    pub injections: Vec<(String, InjectionOrigin)>,
    /// The receipt the message's command asked for, if any.
    pub receipt: Option<atomcode_kernel::event::CommandId>,
}

impl Claimed {
    pub fn is_empty(&self) -> bool {
        self.message.is_none() && self.injections.is_empty()
    }
}

/// What [`Agent::stand_down`] took out of the inbox.
#[derive(Debug, Default)]
pub struct StoodDown {
    /// Messages that will no longer open a turn.
    pub messages: usize,
    /// The receipts their commands asked for. Each is owed an answer: no turn
    /// will claim these messages now.
    pub receipts: Vec<atomcode_kernel::event::CommandId>,
}

/// The single door into an agent.
#[derive(Default)]
pub struct Inbox {
    queue: Mutex<VecDeque<InboxItem>>,
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a message. This is the thing that starts or extends a turn.
    pub fn send(&self, text: impl Into<String>) {
        self.send_full(text, MessageOrigin::User, Vec::new());
    }

    /// Queue a message the harness authored. Wakes the agent like any other
    /// message; logged for what it is.
    pub fn send_from(&self, text: impl Into<String>, origin: MessageOrigin) {
        self.send_full(text, origin, Vec::new());
    }

    /// Queue a message with everything it carries.
    pub fn send_full(
        &self,
        text: impl Into<String>,
        origin: MessageOrigin,
        images: Vec<ImageContent>,
    ) {
        self.send_receipted(text, origin, images, None);
    }

    /// Queue a message whose command wants a receipt: when a turn claims it,
    /// [`crate::events::InputClaimed`] says which turn, carrying `receipt`.
    pub fn send_receipted(
        &self,
        text: impl Into<String>,
        origin: MessageOrigin,
        images: Vec<ImageContent>,
        receipt: Option<atomcode_kernel::event::CommandId>,
    ) {
        self.queue
            .lock()
            .expect("inbox poisoned")
            .push_back(InboxItem::Message {
                text: text.into(),
                origin,
                images,
                receipt,
            });
    }

    /// Queue context to ride along with the next message.
    pub fn inject(&self, text: impl Into<String>, origin: InjectionOrigin) {
        self.queue
            .lock()
            .expect("inbox poisoned")
            .push_back(InboxItem::Injection {
                text: text.into(),
                origin,
            });
    }

    /// Take one message and every injection queued ahead of or alongside it.
    ///
    /// Injections with no message behind them stay put: they are context, and
    /// context does not ask for a turn. Exactly one message is taken, so a
    /// second one becomes the next step rather than being merged into this one —
    /// the model should see them in the order they were sent.
    pub fn claim(&self) -> Claimed {
        let mut queue = self.queue.lock().expect("inbox poisoned");
        if !queue.iter().any(|i| matches!(i, InboxItem::Message { .. })) {
            return Claimed::default();
        }
        let mut claimed = Claimed::default();
        while let Some(item) = queue.pop_front() {
            match item {
                InboxItem::Injection { text, origin } => claimed.injections.push((text, origin)),
                InboxItem::Message {
                    text,
                    origin,
                    images,
                    receipt,
                } => {
                    claimed.message = Some(text);
                    claimed.origin = origin;
                    claimed.images = images;
                    claimed.receipt = receipt;
                    break;
                }
            }
        }
        claimed
    }

    /// Take every injection currently queued, leaving messages alone.
    ///
    /// For the one moment a turn has already committed to running but has not
    /// yet written anything: context queued by a `turn/start` listener belongs
    /// to *this* request, not the next one.
    pub fn claim_injections(&self) -> Vec<(String, InjectionOrigin)> {
        let mut queue = self.queue.lock().expect("inbox poisoned");
        let mut taken = Vec::new();
        queue.retain(|item| match item {
            InboxItem::Injection { text, origin } => {
                taken.push((text.clone(), origin.clone()));
                false
            }
            InboxItem::Message { .. } => true,
        });
        taken
    }

    /// Take every message no turn has claimed yet, leaving injections alone.
    ///
    /// For a person's stop ([`Agent::stand_down`]): a message still waiting here
    /// would open the next turn the moment the stopped one ends.
    pub fn withdraw_messages(&self) -> Vec<InboxItem> {
        let mut queue = self.queue.lock().expect("inbox poisoned");
        let mut taken = Vec::new();
        queue.retain(|item| {
            if matches!(item, InboxItem::Message { .. }) {
                taken.push(item.clone());
                false
            } else {
                true
            }
        });
        taken
    }

    /// Is there a message waiting — something that should wake or extend a turn?
    /// Whether a message from `origin` is waiting.
    pub fn waiting_from(&self, origin: MessageOrigin) -> bool {
        self.queue
            .lock()
            .expect("inbox poisoned")
            .iter()
            .any(|i| matches!(i, InboxItem::Message { origin: o, .. } if *o == origin))
    }

    pub fn has_waking_input(&self) -> bool {
        self.queue
            .lock()
            .expect("inbox poisoned")
            .iter()
            .any(|i| matches!(i, InboxItem::Message { .. }))
    }

    pub fn len(&self) -> usize {
        self.queue.lock().expect("inbox poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A live agent.
///
/// Its `ctx` is its realm: anything registered through it — a tool, a listener,
/// an overridden service — is scoped to this agent and reverts with it, while
/// everything the root installed still applies. That is the same one-way
/// visibility subagents rely on, addressed per agent instead of per subtree.
pub struct Agent {
    id: AgentId,
    ctx: Context,
    inbox: Inbox,
    /// Its own conversation. Provided into its realm as well, so everything
    /// that resolves `sessions` from inside the agent finds this one.
    session: Arc<SessionLog>,
    cwd: Option<PathBuf>,
    parent: Option<String>,
    seed_len: usize,
    persist: bool,
    /// What was mounted for this agent alone, torn down when it is removed.
    world: Mutex<Vec<Disposable>>,
    status: RwLock<AgentStatus>,
    /// Held across a status move and its announcement, so moves are announced
    /// in the order they happened. Separate from `status` so a listener may
    /// still read it.
    moving: Mutex<()>,
    /// The token the *current* turn runs under.
    ///
    /// Per turn, not per agent. A token that outlives the turn it stopped makes
    /// cancellation terminal: the agent is asked to stop once and every turn
    /// after it ends before its first request, which looks exactly like a
    /// broken model. Opening a turn mints a fresh one; cancelling fires the one
    /// in flight.
    cancel: RwLock<CancellationToken>,
    /// Set when the current turn was stopped by a person rather than by the
    /// harness (a reconfigure, a shutdown). Read once, at the turn's end.
    interrupted: std::sync::atomic::AtomicBool,
    /// A person's stop for a turn its driver has already started but that has
    /// not opened yet ([`Agent::interrupt_started`]). The next
    /// [`begin_turn`](Agent::begin_turn) opens that turn already stopped; the
    /// driver clears it once that turn is over ([`Agent::settle_cancel`]).
    cancel_ahead: std::sync::atomic::AtomicBool,
    /// The pump driving this agent, while one is (`docs/adr/0023` §6). Weak: the
    /// agent must not keep its own pump alive — the pump stops when whoever
    /// drives the agent lets go of it.
    commands:
        Mutex<Option<tokio::sync::mpsc::WeakUnboundedSender<atomcode_kernel::event::AgentCommand>>>,
}

impl Agent {
    pub fn id(&self) -> AgentId {
        self.id
    }

    /// This agent's context. Register through it to scope something to this
    /// agent alone.
    pub fn ctx(&self) -> &Context {
        &self.ctx
    }

    pub fn inbox(&self) -> &Inbox {
        &self.inbox
    }

    /// Queue work. Safe to call while a turn is running — the driver claims it
    /// as the next step rather than starting a second turn.
    pub fn send(&self, text: impl Into<String>) {
        self.send_full(text, MessageOrigin::User, Vec::new());
    }

    /// Queue work the harness asked itself for. See [`MessageOrigin`].
    pub fn send_from(&self, text: impl Into<String>, origin: MessageOrigin) {
        self.send_full(text, origin, Vec::new());
    }

    /// Queue a message with its attachments. See [`Inbox::send_full`].
    pub fn send_full(
        &self,
        text: impl Into<String>,
        origin: MessageOrigin,
        images: Vec<ImageContent>,
    ) {
        self.inbox.send_full(text, origin, images);
        self.woke();
    }

    /// Queue a message with a receipt. See [`Inbox::send_receipted`].
    pub fn send_receipted(
        &self,
        text: impl Into<String>,
        origin: MessageOrigin,
        images: Vec<ImageContent>,
        receipt: Option<atomcode_kernel::event::CommandId>,
    ) {
        self.inbox.send_receipted(text, origin, images, receipt);
        self.woke();
    }

    /// Take every queued injection, leaving messages. See [`Inbox::claim_injections`].
    pub fn claim_injections(&self) -> Vec<(String, InjectionOrigin)> {
        self.inbox.claim_injections()
    }

    /// Add model-visible context for the next request this agent makes.
    ///
    /// It lands in the session log when it is claimed, not when it is queued,
    /// so a rejected step does not leave a note in the history claiming the
    /// model saw something it never did.
    pub fn inject(&self, text: impl Into<String>, origin: InjectionOrigin) {
        self.inbox.inject(text, origin);
        self.woke();
    }

    /// Tell this agent something it should know without being woken for it.
    ///
    /// Into its log at once when no turn is running — before the next
    /// `TurnStart`, so whoever is watching sees it now — and queued for the next
    /// turn otherwise: never into a running one, where it could land between a
    /// tool call and its result.
    pub fn note(&self, text: impl Into<String>, origin: InjectionOrigin) {
        let text = text.into();
        {
            let _moving = self.moving.lock().expect("agent status poisoned");
            if self.status() == AgentStatus::Idle {
                crate::session::commit(
                    &self.ctx,
                    &self.session,
                    SessionEvent::Injected {
                        turn: self.session.current_turn(),
                        text,
                        origin,
                    },
                );
                return;
            }
        }
        self.inject(text, origin);
    }

    /// Say the inbox changed. Whether that starts a turn is the driver's call
    /// — an injection alone never does — but the driver has to be told.
    fn woke(&self) {
        self.ctx.emit::<InboxInserted>(&AgentInfo { id: self.id });
    }

    pub fn status(&self) -> AgentStatus {
        *self.status.read().expect("agent status poisoned")
    }

    pub(crate) fn set_status(&self, status: AgentStatus) {
        self.move_status(|_| Some(status));
    }

    /// Move the status if `to` says where, and announce a real move.
    fn move_status(&self, to: impl FnOnce(AgentStatus) -> Option<AgentStatus>) {
        let _moving = self.moving.lock().expect("agent status poisoned");
        let moved = {
            let mut status = self.status.write().expect("agent status poisoned");
            match to(*status) {
                Some(next) if next != *status => {
                    *status = next;
                    true
                }
                _ => false,
            }
        };
        if moved {
            self.ctx.emit::<AgentStatusChanged>(&self.change());
        }
    }

    fn change(&self) -> AgentChange {
        AgentChange {
            id: self.id,
            session: self.session_id().to_string(),
            parent: self.parent.clone(),
            status: self.status(),
        }
    }

    /// This agent as a front end is told about it (`docs/adr/0022` §5).
    ///
    /// What the agent's realm resolves is filled here; everything a row gave
    /// this agent is written by that row, through [`DescribeAgent`].
    pub fn describe(&self) -> AgentDescription {
        let model = self.ctx.service::<LlmSvc>();
        let describing = Describing {
            description: Mutex::new(AgentDescription {
                session: self.session_id().to_string(),
                parent: self.parent.clone(),
                member: None,
                model: model.as_ref().map(|m| m.model_name().to_string()),
                context_window: model.as_ref().map(|m| m.context_window()),
                supports_vision: model.as_ref().is_some_and(|m| m.supports_vision()),
                reasoning_effort: None,
                compaction: self.ctx.service::<CompactionSvc>().is_some(),
                commands: self
                    .ctx
                    .service::<crate::seams::CommandsSvc>()
                    .map(|catalog| catalog.offered_for(self))
                    .unwrap_or_default(),
            }),
        };
        self.ctx.emit::<DescribeAgent>(&describing);
        describing
            .description
            .into_inner()
            .expect("description poisoned")
    }

    /// Hand a command to the pump driving this agent. `false` when nothing
    /// drives it.
    pub fn command(&self, command: atomcode_kernel::event::AgentCommand) -> bool {
        let sender = self
            .commands
            .lock()
            .expect("agent commands poisoned")
            .as_ref()
            .and_then(|weak| weak.upgrade());
        sender.is_some_and(|sender| sender.send(command).is_ok())
    }

    /// Whether a pump drives this agent.
    pub fn is_driven(&self) -> bool {
        self.commands
            .lock()
            .expect("agent commands poisoned")
            .as_ref()
            .is_some_and(|weak| weak.upgrade().is_some())
    }

    pub(crate) fn attach_commands(
        &self,
        sender: &tokio::sync::mpsc::UnboundedSender<atomcode_kernel::event::AgentCommand>,
    ) {
        *self.commands.lock().expect("agent commands poisoned") = Some(sender.downgrade());
    }

    pub(crate) fn detach_commands(&self) {
        self.commands
            .lock()
            .expect("agent commands poisoned")
            .take();
    }

    /// Ask the current turn to stop. Cooperative: the step in flight finishes.
    ///
    /// A no-op when nothing is running, which is what cancelling an idle agent
    /// means. It does not queue: a cancel that arrives before a turn opens is
    /// not held against that turn.
    pub fn cancel(&self) {
        // Only a turn can be stopping. An idle agent marked stopping stayed so
        // until its next turn opened, and a front end showed it stopping all
        // that while.
        self.move_status(|now| (now == AgentStatus::Working).then_some(AgentStatus::Stopping));
        self.cancel.read().expect("cancel token poisoned").cancel();
    }

    /// Cancel on a person's behalf: the turn stops, and the history is told so.
    /// [`Agent::cancel`] is the harness stopping a turn for its own reasons
    /// (reconfiguring, shutting down), which abandons nothing.
    pub fn interrupt(&self) {
        if self.status() == AgentStatus::Working {
            self.interrupted
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.cancel();
    }

    /// [`interrupt`](Self::interrupt), from a driver that knows it has started
    /// a turn: the stop holds for that turn even if it has not opened yet.
    ///
    /// Between a driver spawning a turn and the turn calling
    /// [`begin_turn`](Self::begin_turn) the agent still reads as idle, so a plain
    /// `interrupt` there fired the idle token that `begin_turn` then replaced —
    /// and the turn ran to its end under a fresh one while the person watched
    /// "stopping". Enter then esc lands in exactly that window.
    pub fn interrupt_started(&self) {
        self.cancel_ahead
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.interrupt();
    }

    /// The started turn is over: a stop held for it is not held against the
    /// next one. The driver's call, because only the driver knows when the turn
    /// it started — opened or not — is done.
    pub fn settle_cancel(&self) {
        self.cancel_ahead
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// What a person's stop does to what is waiting: nothing queued goes on to
    /// open a turn of its own.
    ///
    /// Their own messages, and the harness's continuations and nudges, are
    /// withdrawn — each would have started the next turn the instant the
    /// stopped one ended, which is the agent carrying on after being told to
    /// stop. (The engine this replaced cleared its steer buffer on cancel, and
    /// front ends still clear their steering panel on the same understanding.)
    /// A peer's report is kept, as a note for the next turn rather than a
    /// reason to start one: it is information the person did not write and has
    /// not seen.
    pub fn stand_down(&self) -> StoodDown {
        let mut stood = StoodDown::default();
        for item in self.inbox.withdraw_messages() {
            let InboxItem::Message {
                text,
                origin,
                receipt,
                ..
            } = item
            else {
                continue;
            };
            stood.messages += 1;
            stood.receipts.extend(receipt);
            if let MessageOrigin::Peer(sender) = origin {
                let from = self
                    .ctx
                    .service::<crate::seams::AgentsSvc>()
                    .and_then(|agents| agents.get(sender))
                    .map(|agent| agent.session_id().to_string())
                    .unwrap_or_else(|| format!("agent-{sender}"));
                self.note(text, InjectionOrigin::Peer { from });
            }
        }
        stood
    }

    /// Whether a person interrupted the turn now ending. Clears the mark.
    pub fn take_interrupted(&self) -> bool {
        self.interrupted
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    pub fn cancelled(&self) -> bool {
        self.cancel
            .read()
            .expect("cancel token poisoned")
            .is_cancelled()
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.read().expect("cancel token poisoned").clone()
    }

    /// Open a turn: mint this turn's cancellation token and mark the agent
    /// working. Returns the token, so a driver holds the same one `cancel`
    /// fires.
    ///
    /// Every loop implementation calls this, because "which token is this turn
    /// running under" is the agent's fact, not the driver's.
    pub fn begin_turn(&self) -> CancellationToken {
        self.interrupted
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let fresh = CancellationToken::new();
        *self.cancel.write().expect("cancel token poisoned") = fresh.clone();
        self.set_status(AgentStatus::Working);
        // Stopped before it opened: it opens stopped, and the person's stop is
        // recorded as theirs.
        if self
            .cancel_ahead
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.interrupt();
        }
        fresh
    }

    /// Close a turn: back to idle, under a fresh token.
    ///
    /// The pair with [`begin_turn`](Self::begin_turn), and the reason an idle
    /// agent is never cancelled. Leaving the fired token in place would make
    /// `cancelled()` answer a question about a turn that has already ended, and
    /// every observer between turns would read the agent as stopping.
    pub fn end_turn(&self) {
        *self.cancel.write().expect("cancel token poisoned") = CancellationToken::new();
        self.set_status(AgentStatus::Idle);
    }

    /// The log this agent writes to — its own if its realm provides one, else
    /// the one its parent realm provides.
    pub fn session(&self) -> Arc<SessionLog> {
        self.session.clone()
    }
    pub fn session_id(&self) -> &str {
        self.session.id()
    }
    pub fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }
    pub fn parent(&self) -> Option<&str> {
        self.parent.as_deref()
    }
    /// How many of the log's events were inherited (fork) or restored (resume).
    pub fn seed_len(&self) -> usize {
        self.seed_len
    }
    pub fn persist(&self) -> bool {
        self.persist
    }
}

/// The live agent registry — cordis's `ctx.agents`.
///
/// A service rather than a field on something, so a UI, a webhook, a scheduler
/// or another agent can find the agents running in this process without any of
/// them knowing about each other.
#[derive(Default)]
pub struct Agents {
    next_id: AtomicU64,
    agents: RwLock<BTreeMap<AgentId, Arc<Agent>>>,
}

impl Agents {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an agent in a realm of its own, layered over `ctx`.
    ///
    /// The realm is created eagerly and unconditionally: an agent that shares
    /// its parent's realm would be an agent whose per-agent registrations are
    /// not per-agent, and that difference is invisible until something breaks.
    /// Create an agent with its own log and world, and announce it — in that
    /// order. Nothing observes the agent until everything it was given is in
    /// place, so an `agent/created` listener never sees a half-composed one.
    pub async fn create(&self, ctx: &Context, mut req: CreateAgent) -> Result<Arc<Agent>, String> {
        let realm = ctx.isolate();
        let session_id = req.id.take().unwrap_or_else(mint_session_id);

        let mut seed = std::mem::take(&mut req.seed);
        let mut seed_len = req.seed_len;
        // The identity the session is created with. A fork records its parent
        // and how much of the seed is the parent's; a resume keeps the header
        // the session was created with, which is the whole point of storing
        // one; a session started empty gets a fresh one.
        let mut header = SessionHeader::new(session_id.clone());
        header.cwd = req.cwd.as_ref().map(|p| p.display().to_string());
        header.parent = req.parent.clone();
        header.member = req.member.take();
        if req.parent.is_some() {
            header.inherited = seed_len;
        }
        // A seed for a session the store already holds events under continues
        // the store's numbering. Without this, the events this agent commits are
        // appended with sequence numbers the file already used, and a
        // compaction boundary written later cuts a different place on replay.
        if !seed.is_empty() && req.persist && req.parent.is_none() {
            if let Some(store) = ctx.service::<SessionPersistenceSvc>() {
                let stored = store.load(&session_id).await.unwrap_or_default();
                if let Some(max) = stored.iter().map(|e| e.seq).max() {
                    seed = crate::session::renumber(seed, max + 1);
                }
            }
        }
        if req.resume && seed.is_empty() {
            let store = ctx
                .service::<SessionPersistenceSvc>()
                .ok_or("resume: no `session-persistence` is mounted")?;
            if let Some(stored) = store.header(&session_id).await? {
                header = stored;
            }
            seed = store.load(&session_id).await?;
            seed_len = seed.len();
        }
        let clock: Arc<dyn atomcode_kernel::clock::WallClock> = ctx
            .service::<crate::seams::WallClockSvc>()
            .unwrap_or_else(|| Arc::new(atomcode_kernel::clock::SystemWallClock));
        let log = Arc::new(SessionLog::with_clock(header, clock));
        if !seed.is_empty() {
            log.restore(seed);
        }

        let mut world: Vec<Disposable> = Vec::new();
        let tear_down = |world: Vec<Disposable>| {
            for d in world {
                d.dispose();
            }
        };
        match realm.provide::<SessionSvc>(log.clone()) {
            Ok(d) => world.push(d),
            Err(e) => return Err(e.to_string()),
        }
        if let Some(cwd) = &req.cwd {
            let fs: Arc<dyn crate::seams::FileSystem> =
                Arc::new(atomcode_capabilities::world::LocalFs::new(cwd.clone()));
            match realm.provide::<FsSvc>(fs) {
                Ok(d) => world.push(d),
                Err(e) => {
                    tear_down(world);
                    return Err(e.to_string());
                }
            }
        }
        if let Some(setup) = req.setup.take() {
            match setup(&realm) {
                Ok(more) => world.extend(more),
                Err(e) => {
                    tear_down(world);
                    return Err(e);
                }
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let agent = Arc::new(Agent {
            id,
            ctx: realm,
            inbox: Inbox::new(),
            session: log,
            cwd: req.cwd,
            parent: req.parent,
            seed_len,
            persist: req.persist,
            world: Mutex::new(world),
            status: RwLock::new(AgentStatus::Idle),
            moving: Mutex::new(()),
            cancel: RwLock::new(CancellationToken::new()),
            interrupted: std::sync::atomic::AtomicBool::new(false),
            cancel_ahead: std::sync::atomic::AtomicBool::new(false),
            commands: Mutex::new(None),
        });
        self.agents
            .write()
            .expect("agent registry poisoned")
            .insert(id, agent.clone());
        ctx.emit::<AgentCreated>(&AgentInfo { id });
        Ok(agent)
    }

    /// The agent whose session this is, if it is live.
    pub fn by_session(&self, session_id: &str) -> Option<Arc<Agent>> {
        self.agents
            .read()
            .expect("agent registry poisoned")
            .values()
            .find(|a| a.session_id() == session_id)
            .cloned()
    }

    pub fn get(&self, id: AgentId) -> Option<Arc<Agent>> {
        self.agents
            .read()
            .expect("agent registry poisoned")
            .get(&id)
            .cloned()
    }

    pub fn list(&self) -> Vec<Arc<Agent>> {
        self.agents
            .read()
            .expect("agent registry poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Drop an agent from the registry. Its realm's registrations are reverted
    /// by whoever owns the fibers mounted into it.
    /// Forget an agent and tear down what was mounted for it alone.
    pub fn remove(&self, id: AgentId) -> Option<Arc<Agent>> {
        let removed = self
            .agents
            .write()
            .expect("agent registry poisoned")
            .remove(&id);
        if let Some(agent) = &removed {
            let world: Vec<Disposable> =
                std::mem::take(&mut *agent.world.lock().expect("agent world poisoned"));
            for d in world {
                d.dispose();
            }
            agent.ctx.emit::<AgentRemoved>(&agent.change());
        }
        removed
    }

    pub fn len(&self) -> usize {
        self.agents.read().expect("agent registry poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
