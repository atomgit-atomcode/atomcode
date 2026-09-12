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

use crate::events::{AgentCreated, AgentInfo};
use crate::seams::{FsSvc, SessionDefaultsSvc, SessionPersistenceSvc, SessionSvc};
use crate::session::{InjectionOrigin, LoggedEvent, SessionLog};

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
    /// Whether the persistence seam should keep this session. A delegated
    /// child's transcript is its parent's business, not a session of its own.
    pub persist: bool,
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
        Self {
            id: defaults.as_ref().and_then(|d| d.id.clone()),
            resume: defaults.map(|d| d.resume).unwrap_or(false),
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
    pub fn setup(mut self, setup: Setup) -> Self {
        self.setup = Some(setup);
        self
    }
}

pub type AgentId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// Nothing owed; waiting for input.
    Idle,
    /// A turn is open.
    Working,
    /// Cancellation asked for; the current step is finishing.
    Stopping,
}

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
}

impl Claimed {
    pub fn is_empty(&self) -> bool {
        self.message.is_none() && self.injections.is_empty()
    }
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
        self.queue
            .lock()
            .expect("inbox poisoned")
            .push_back(InboxItem::Message {
                text: text.into(),
                origin,
                images,
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
                } => {
                    claimed.message = Some(text);
                    claimed.origin = origin;
                    claimed.images = images;
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
                taken.push((text.clone(), *origin));
                false
            }
            InboxItem::Message { .. } => true,
        });
        taken
    }

    /// Is there a message waiting — something that should wake or extend a turn?
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
    /// The token the *current* turn runs under.
    ///
    /// Per turn, not per agent. A token that outlives the turn it stopped makes
    /// cancellation terminal: the agent is asked to stop once and every turn
    /// after it ends before its first request, which looks exactly like a
    /// broken model. Opening a turn mints a fresh one; cancelling fires the one
    /// in flight.
    cancel: RwLock<CancellationToken>,
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
        self.inbox.send(text);
    }

    /// Queue work the harness asked itself for. See [`MessageOrigin`].
    pub fn send_from(&self, text: impl Into<String>, origin: MessageOrigin) {
        self.inbox.send_from(text, origin);
    }

    /// Queue a message with its attachments. See [`Inbox::send_full`].
    pub fn send_full(
        &self,
        text: impl Into<String>,
        origin: MessageOrigin,
        images: Vec<ImageContent>,
    ) {
        self.inbox.send_full(text, origin, images);
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
    }

    pub fn status(&self) -> AgentStatus {
        *self.status.read().expect("agent status poisoned")
    }

    pub(crate) fn set_status(&self, status: AgentStatus) {
        *self.status.write().expect("agent status poisoned") = status;
    }

    /// Ask the current turn to stop. Cooperative: the step in flight finishes.
    ///
    /// A no-op when nothing is running, which is what cancelling an idle agent
    /// means. It does not queue: a cancel that arrives before a turn opens is
    /// not held against that turn.
    pub fn cancel(&self) {
        self.set_status(AgentStatus::Stopping);
        self.cancel.read().expect("cancel token poisoned").cancel();
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
        let fresh = CancellationToken::new();
        *self.cancel.write().expect("cancel token poisoned") = fresh.clone();
        self.set_status(AgentStatus::Working);
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
        if req.resume && seed.is_empty() {
            let store = ctx
                .service::<SessionPersistenceSvc>()
                .ok_or("resume: no `session-persistence` is mounted")?;
            seed = store.load(&session_id).await?;
            seed_len = seed.len();
        }
        let log = Arc::new(SessionLog::new(session_id));
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
            cancel: RwLock::new(CancellationToken::new()),
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
