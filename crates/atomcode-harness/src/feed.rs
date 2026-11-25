//! A session pushed to whoever subscribed to it: the agent behind it described,
//! where it stands, its members as they come and go, and its facts from a
//! sequence number on (`docs/adr/0022` §1, §5).
//!
//! One implementation, two users. The handle pump answers a driver's
//! `Subscribe` with it; a host whose front end lives outside the App — and
//! outlives it, when the host replaces the App under a new session — keeps one
//! across Apps and attaches it to each (`docs/adr/0022` §3). The two cannot
//! drift on the one property both owe a subscriber: history and live facts meet
//! with no gap and no repeat.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use atomcode_kernel::event::{AgentEvent, CommandError};
use atomcode_plexus::{Context, Disposable};
use tokio::sync::mpsc;

use crate::agent::Agent;
use crate::events::{
    AgentChange, AgentCreated, AgentInfo, AgentRemoved, AgentStatusChanged, SessionEventCommitted,
};
use crate::seams::AgentsSvc;
use crate::session::{Committed, SeqNo};

/// What one subscribed session's subscriber has been sent so far.
struct Subscription {
    /// The last fact.
    high: SeqNo,
    /// The members it has been told joined and not yet told left. Only these
    /// are reported on, so a member is never heard of before it is added or
    /// after it is removed, and never added twice.
    members: HashSet<String>,
    /// Whether the subscriber has been told what it follows (`Described`).
    /// `false` for one asked for before any App had the session — see
    /// [`Feed::expect`] — and nothing about it is pushed until an agent for it
    /// arrives and it is opened.
    open: bool,
}

/// Sessions pushed into one event channel.
pub struct Feed {
    events: mpsc::UnboundedSender<AgentEvent>,
    /// Held across a subscription's catch-up, and by every listener while it
    /// decides: a fact committed, a member added or a status moved during the
    /// catch-up waits here and then compares against what was recorded.
    subscriptions: Mutex<HashMap<String, Subscription>>,
}

impl Feed {
    pub fn new(events: mpsc::UnboundedSender<AgentEvent>) -> Arc<Self> {
        Arc::new(Self {
            events,
            subscriptions: Mutex::new(HashMap::new()),
        })
    }

    /// The live agent a subscription to `session` would follow.
    pub fn find(ctx: &Context, session: &str) -> Option<Arc<Agent>> {
        ctx.service::<AgentsSvc>()?.by_session(session)
    }

    /// Start pushing `agent`'s session: described, where it stands, each member
    /// already there, then every fact from `from` on — and from here on,
    /// whatever the listeners [`attach`](Self::attach)ed hear about it.
    pub fn subscribe(&self, ctx: &Context, agent: &Agent, from: SeqNo) {
        let session = agent.session_id().to_string();
        let mut subscribed = self.subscriptions.lock().expect("subscriptions poisoned");
        let mut subscription = Subscription {
            high: from.saturating_sub(1),
            members: HashSet::new(),
            open: false,
        };
        let agents = ctx.service::<AgentsSvc>();
        self.open(&mut subscription, agent, agents.as_deref());
        subscribed.insert(session, subscription);
    }

    /// Follow `session` from `from` on, though no App has it yet.
    ///
    /// A host can be up with no App at all: its runtime is waiting for a
    /// provider (none configured, a login that lapsed, a gateway this build
    /// cannot sign for), and the App is only mounted once one can serve. A
    /// front end subscribes when it opens, which is before that — and a
    /// subscription refused then was never asked for again, so the screen was
    /// fed nothing of the session it was showing once the provider arrived.
    /// Kept here instead, it is opened by the first App that has the session,
    /// exactly as [`subscribe`](Self::subscribe) would have opened it.
    pub fn expect(&self, session: &str, from: SeqNo) {
        self.subscriptions
            .lock()
            .expect("subscriptions poisoned")
            .entry(session.to_string())
            .or_insert(Subscription {
                high: from.saturating_sub(1),
                members: HashSet::new(),
                open: false,
            });
    }

    /// Open an [`expect`](Self::expect)ed subscription to `session` now, if
    /// `ctx` has its agent and nothing has opened it yet.
    ///
    /// For the host that expected it and then found an App after all: the App
    /// may have attached — and scanned for waiting subscriptions — in between,
    /// and would then never open this one. Decided under the same lock the
    /// scan takes, so it is opened exactly once whichever got there first.
    pub fn open_expected(&self, ctx: &Context, session: &str) {
        let Some(agent) = Self::find(ctx, session) else {
            return;
        };
        let agents = ctx.service::<AgentsSvc>();
        let mut subscribed = self.subscriptions.lock().expect("subscriptions poisoned");
        if let Some(subscription) = subscribed.get_mut(session).filter(|s| !s.open) {
            self.open(subscription, &agent, agents.as_deref());
        }
    }

    /// Tell the subscriber what it follows: the agent described, where it
    /// stands, each member, then every fact after `high`.
    fn open(
        &self,
        subscription: &mut Subscription,
        agent: &Agent,
        agents: Option<&crate::agent::Agents>,
    ) {
        let session = agent.session_id().to_string();
        subscription.open = true;
        let _ = self.events.send(AgentEvent::Described {
            description: Box::new(agent.describe()),
        });
        let _ = self.events.send(AgentEvent::StatusChanged {
            session: session.clone(),
            status: agent.status(),
        });
        for member in agents.iter().flat_map(|a| a.list()) {
            if member.parent() == Some(session.as_str()) {
                self.announce(subscription, &member);
            }
        }
        self.catch_up(subscription, agent);
    }

    /// [`find`](Self::find), then [`subscribe`](Self::subscribe).
    pub fn subscribe_to(
        &self,
        ctx: &Context,
        session: &str,
        from: SeqNo,
    ) -> Result<(), CommandError> {
        let agent = Self::find(ctx, session).ok_or(CommandError::NotFound)?;
        self.subscribe(ctx, &agent, from);
        Ok(())
    }

    /// A delegated agent that is gone, from the log its store kept
    /// (`docs/adr/0023` §5): described as its header records it, then every
    /// fact from `from` on. Nothing follows — it will say nothing more.
    ///
    /// Only a delegated session: another conversation is not reachable by id
    /// from this one.
    pub async fn replay_kept(
        &self,
        ctx: &Context,
        session: &str,
        from: SeqNo,
    ) -> Result<(), CommandError> {
        let store = ctx
            .service::<crate::seams::SessionPersistenceSvc>()
            .ok_or(CommandError::NotFound)?;
        let header = store
            .header(session)
            .await
            .ok()
            .flatten()
            .filter(|header| header.parent.is_some())
            .ok_or(CommandError::NotFound)?;
        let events = store
            .load(session)
            .await
            .map_err(|_| CommandError::Unavailable)?;
        let _ = self.events.send(AgentEvent::Described {
            description: Box::new(atomcode_kernel::agent::AgentDescription {
                session: session.to_string(),
                parent: header.parent.clone(),
                working_dir: header.cwd,
                member: header
                    .member
                    .as_ref()
                    .map(|m| atomcode_kernel::agent::MemberIdentity {
                        name: m.name.clone(),
                        role: m.role.clone(),
                    }),
                ..Default::default()
            }),
        });
        for logged in events.into_iter().filter(|logged| logged.seq >= from) {
            let _ = self.events.send(AgentEvent::Fact(Box::new(Committed {
                session: session.to_string(),
                seq: logged.seq,
                at: logged.at,
                event: logged.event,
            })));
        }
        Ok(())
    }

    pub fn unsubscribe(&self, session: &str) {
        self.subscriptions
            .lock()
            .expect("subscriptions poisoned")
            .remove(session);
    }

    /// Forget every subscription — the sessions they followed were replaced.
    pub fn clear(&self) {
        self.subscriptions
            .lock()
            .expect("subscriptions poisoned")
            .clear();
    }

    /// Describe each subscribed session's agent again: something it is
    /// described by changed.
    pub fn redescribe(&self, ctx: &Context) {
        let sessions = self.subscriptions.lock().expect("subscriptions poisoned");
        for session in sessions.keys() {
            if let Some(agent) = Self::find(ctx, session) {
                let _ = self.events.send(AgentEvent::Described {
                    description: Box::new(agent.describe()),
                });
            }
        }
    }

    /// Listen on `ctx` — a tree every agent's realm announces up to — for what
    /// the subscriptions follow. Dispose what comes back to stop.
    pub fn attach(self: &Arc<Self>, ctx: &Context) -> Vec<Disposable> {
        let mut listening = Vec::new();

        let feed = self.clone();
        listening.push(
            ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
                let mut sessions = feed.subscriptions.lock().expect("subscriptions poisoned");
                if let Some(subscription) = sessions.get_mut(&committed.session) {
                    if subscription.open && committed.seq > subscription.high {
                        subscription.high = committed.seq;
                        let _ = feed
                            .events
                            .send(AgentEvent::Fact(Box::new(committed.clone())));
                    }
                }
            }),
        );

        if let Some(registry) = ctx.service::<AgentsSvc>() {
            let feed = self.clone();
            let agents = registry.clone();
            listening.push(ctx.on_emit::<AgentCreated>(move |info: &AgentInfo| {
                let Some(created) = agents.get(info.id) else {
                    return;
                };
                let mut sessions = feed.subscriptions.lock().expect("subscriptions poisoned");
                if let Some(subscription) = sessions.get_mut(created.session_id()) {
                    feed.catch_up_or_open(subscription, &created, &agents);
                }
                let Some(parent) = created.parent() else {
                    return;
                };
                if let Some(subscription) = sessions.get_mut(parent).filter(|s| s.open) {
                    feed.announce(subscription, &created);
                }
            }));
            // An agent this App already had when the feed attached.
            let mut sessions = self.subscriptions.lock().expect("subscriptions poisoned");
            for agent in registry.list() {
                if let Some(subscription) = sessions.get_mut(agent.session_id()) {
                    self.catch_up_or_open(subscription, &agent, &registry);
                }
            }
        }

        let feed = self.clone();
        listening.push(ctx.on_emit::<AgentRemoved>(move |gone: &AgentChange| {
            let Some(parent) = &gone.parent else {
                return;
            };
            let mut sessions = feed.subscriptions.lock().expect("subscriptions poisoned");
            if let Some(subscription) = sessions.get_mut(parent) {
                if subscription.members.remove(&gone.session) {
                    let _ = feed.events.send(AgentEvent::AgentRemoved {
                        session: gone.session.clone(),
                    });
                }
            }
        }));

        let feed = self.clone();
        listening.push(
            ctx.on_emit::<AgentStatusChanged>(move |change: &AgentChange| {
                let sessions = feed.subscriptions.lock().expect("subscriptions poisoned");
                let subscribed = sessions
                    .get(&change.session)
                    .is_some_and(|subscription| subscription.open)
                    || change
                        .parent
                        .as_ref()
                        .and_then(|parent| sessions.get(parent))
                        .is_some_and(|subscription| subscription.members.contains(&change.session));
                if subscribed {
                    let _ = feed.events.send(AgentEvent::StatusChanged {
                        session: change.session.clone(),
                        status: change.status,
                    });
                }
            }),
        );

        listening
    }

    /// [`catch_up`](Self::catch_up) an open subscription, [`open`](Self::open)
    /// one that was only [`expect`](Self::expect)ed.
    fn catch_up_or_open(
        &self,
        subscription: &mut Subscription,
        agent: &Agent,
        agents: &crate::agent::Agents,
    ) {
        if subscription.open {
            self.catch_up(subscription, agent);
        } else {
            self.open(subscription, agent, Some(agents));
        }
    }

    /// The facts of a subscribed session that an App brought with it and nobody
    /// committed in front of this feed: a host that rebuilds its App for the
    /// same session builds it on the log, and whatever the log gained between
    /// the two Apps — an undo the old one could not say live, appended to the
    /// store for the new one to replay — arrives here and nowhere else. Without
    /// it the subscriber's stream has a gap, and what it is based on is behind
    /// a log it was never shown (a front end's undo refused as stale).
    fn catch_up(&self, subscription: &mut Subscription, agent: &Agent) {
        let session = agent.session_id().to_string();
        // Only what is past the subscriber's mark: copying the whole log to skip
        // most of it costs a conversation per catch-up.
        for logged in agent.session().since(subscription.high) {
            subscription.high = logged.seq;
            let _ = self.events.send(AgentEvent::Fact(Box::new(Committed {
                session: session.clone(),
                seq: logged.seq,
                at: logged.at,
                event: logged.event,
            })));
        }
    }

    /// A member, described and then where it stands — once.
    fn announce(&self, subscription: &mut Subscription, member: &Agent) {
        if subscription.members.insert(member.session_id().to_string()) {
            let _ = self.events.send(AgentEvent::AgentAdded {
                description: Box::new(member.describe()),
            });
            let _ = self.events.send(AgentEvent::StatusChanged {
                session: member.session_id().to_string(),
                status: member.status(),
            });
        }
    }
}
