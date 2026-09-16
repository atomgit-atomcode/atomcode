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
        };
        let _ = self.events.send(AgentEvent::Described {
            description: Box::new(agent.describe()),
        });
        let _ = self.events.send(AgentEvent::StatusChanged {
            session: session.clone(),
            status: agent.status(),
        });
        for member in ctx.service::<AgentsSvc>().iter().flat_map(|a| a.list()) {
            if member.parent() == Some(session.as_str()) {
                self.announce(&mut subscription, &member);
            }
        }
        for logged in agent.session().events() {
            if logged.seq >= from {
                subscription.high = logged.seq;
                let _ = self.events.send(AgentEvent::Fact(Box::new(Committed {
                    session: session.clone(),
                    seq: logged.seq,
                    event: logged.event,
                })));
            }
        }
        subscribed.insert(session, subscription);
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
                    if committed.seq > subscription.high {
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
            listening.push(ctx.on_emit::<AgentCreated>(move |info: &AgentInfo| {
                let Some(member) = registry.get(info.id) else {
                    return;
                };
                let Some(parent) = member.parent() else {
                    return;
                };
                let mut sessions = feed.subscriptions.lock().expect("subscriptions poisoned");
                if let Some(subscription) = sessions.get_mut(parent) {
                    feed.announce(subscription, &member);
                }
            }));
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
                let subscribed = sessions.contains_key(&change.session)
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
