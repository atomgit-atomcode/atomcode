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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use atomcode_plexus::Context;
use tokio_util::sync::CancellationToken;

use crate::seams::SessionSvc;
use crate::session::{InjectionOrigin, SessionLog};

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

/// One item waiting for an agent.
#[derive(Clone, Debug, PartialEq)]
pub enum InboxItem {
    /// Someone asking for work. Wakes an idle agent.
    Message { text: String },
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
        self.queue
            .lock()
            .expect("inbox poisoned")
            .push_back(InboxItem::Message { text: text.into() });
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
                InboxItem::Message { text } => {
                    claimed.message = Some(text);
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
    status: RwLock<AgentStatus>,
    cancel: CancellationToken,
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
    pub fn cancel(&self) {
        self.set_status(AgentStatus::Stopping);
        self.cancel.cancel();
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// The log this agent writes to — its own if its realm provides one, else
    /// the one its parent realm provides.
    pub fn session(&self) -> Option<Arc<SessionLog>> {
        self.ctx.service::<SessionSvc>()
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
    pub fn create(&self, ctx: &Context) -> Arc<Agent> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let agent = Arc::new(Agent {
            id,
            ctx: ctx.isolate(),
            inbox: Inbox::new(),
            status: RwLock::new(AgentStatus::Idle),
            cancel: CancellationToken::new(),
        });
        self.agents
            .write()
            .expect("agent registry poisoned")
            .insert(id, agent.clone());
        agent
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
    pub fn remove(&self, id: AgentId) -> Option<Arc<Agent>> {
        self.agents
            .write()
            .expect("agent registry poisoned")
            .remove(&id)
    }

    pub fn len(&self) -> usize {
        self.agents.read().expect("agent registry poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
