//! A host for agents built from a config tree, speaking both front-end
//! contracts (`docs/adr/0021`, `docs/adr/0022`).
//!
//! A front end that lives in an App of its own — the full-screen UI — does not
//! build the agent it drives. It is handed a [`HostConnection`]: the handle
//! protocol to the live agent, and [`HostControl`] over it. This is the host for
//! a tree the harness can mount by itself; the product's coding runtime is
//! another, with the same connection on the far side.
//!
//! It changes session the way the product runtime does today: it mounts a new
//! App for the new session and retires the old one. The front end keeps its
//! channels throughout and learns of the change as the session's identity
//! changing, never as an App being rebuilt (`docs/adr/0022` §2).
//!
//! The tree must carry `ui-handle`: the agent is driven through the same pump
//! every other driver uses, and a tree without one has nothing to connect to.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::host::{
    HostCommand, HostConnection, HostControl, HostError, HostEvent, HostReply, StoredSession,
};
use atomcode_kernel::provider::ReasoningEffort;
use atomcode_plexus::{App, ConfigTree, Layer, PluginRegistry};
use tokio::sync::mpsc;

use crate::seams::{AgentHandleSvc, AgentsSvc, SessionPersistenceSvc};

/// Which session a tree is built for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Opening {
    /// A new, empty session.
    Fresh,
    /// A stored session, by id.
    Resume(String),
}

/// The plugins every App is built from.
pub type Registry = Arc<dyn Fn() -> PluginRegistry + Send + Sync>;

/// The tree for an opening. The caller decides what resuming means in its tree
/// — usually [`crate::bundle::resume_overlay`] on top of its layers.
pub type Trees = Arc<dyn Fn(&Opening) -> Result<ConfigTree, String> + Send + Sync>;

/// The App now running, and how to reach its agent.
struct Live {
    app: App,
    commands: mpsc::UnboundedSender<AgentCommand>,
    session: String,
    relay: tokio::task::JoinHandle<()>,
}

struct TreeHost {
    registry: Registry,
    trees: Trees,
    /// Held across a session change, so no command reaches an agent that is
    /// being retired.
    live: tokio::sync::Mutex<Option<Live>>,
    events: mpsc::UnboundedSender<AgentEvent>,
    watchers: Mutex<Vec<mpsc::UnboundedSender<HostEvent>>>,
    /// Whether the live agent is in a turn, as its own events say. A session is
    /// not replaced under a running turn: what that turn goes on to say would
    /// arrive after the front end had moved to the next session.
    turning: Arc<AtomicBool>,
}

/// Mount the tree for `first` and connect to its agent.
pub async fn open(
    registry: Registry,
    trees: Trees,
    first: Opening,
) -> Result<HostConnection, String> {
    let (events, event_rx) = mpsc::unbounded_channel();
    let (commands, mut command_rx) = mpsc::unbounded_channel::<AgentCommand>();
    let host = Arc::new(TreeHost {
        registry,
        trees,
        live: tokio::sync::Mutex::new(None),
        events,
        watchers: Mutex::new(Vec::new()),
        turning: Arc::new(AtomicBool::new(false)),
    });
    let live = host.mount(&first).await.map_err(|e| format!("{e:?}"))?;
    let session = live.session.clone();
    *host.live.lock().await = Some(live);

    let relay = host.clone();
    tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            if let Some(live) = relay.live.lock().await.as_ref() {
                let _ = live.commands.send(command);
            }
        }
        // The front end hung up: so does the agent.
        let retiring = relay.live.lock().await.take();
        if let Some(live) = retiring {
            retire(live).await;
        }
    });

    Ok(HostConnection {
        session,
        commands,
        events: event_rx,
        control: host,
    })
}

impl TreeHost {
    async fn mount(&self, opening: &Opening) -> Result<Live, HostError> {
        let failed = |message: String| HostError::Failed { message };
        let tree = (self.trees)(opening).map_err(failed)?;
        let mut app = App::new((self.registry)(), tree);
        app.start().await.map_err(|e| failed(e.to_string()))?;
        let ctx = app.context();
        let handle = ctx.service::<AgentHandleSvc>().and_then(|h| h.take());
        let agent = ctx
            .service::<AgentsSvc>()
            .and_then(|agents| agents.list().into_iter().find(|a| a.parent().is_none()));
        let (Some(handle), Some(agent)) = (handle, agent) else {
            app.stop();
            return Err(failed(
                "this tree has no `ui-handle` agent to connect to".to_string(),
            ));
        };
        let AgentHandle {
            commands,
            mut events,
            ..
        } = handle;
        let out = self.events.clone();
        let turning = self.turning.clone();
        turning.store(false, Ordering::SeqCst);
        let relay = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match &event {
                    AgentEvent::TurnStarted { .. } => turning.store(true, Ordering::SeqCst),
                    AgentEvent::TurnComplete { .. } => turning.store(false, Ordering::SeqCst),
                    _ => {}
                }
                if out.send(event).is_err() {
                    break;
                }
            }
        });
        Ok(Live {
            app,
            commands,
            session: agent.session_id().to_string(),
            relay,
        })
    }

    /// Put the session `opening` names in place of `addressed`.
    async fn replace(&self, addressed: &str, opening: Opening) -> Result<HostReply, HostError> {
        let mut live = self.live.lock().await;
        let current = live.as_ref().ok_or(HostError::Unavailable)?;
        if current.session != addressed {
            return Err(HostError::NotFound);
        }
        if self.turning.load(Ordering::SeqCst) {
            return Err(HostError::Busy {
                reason: "a turn is running".into(),
            });
        }
        if let Opening::Resume(target) = &opening {
            if *target == current.session {
                return Err(HostError::SessionInUse { id: target.clone() });
            }
            let store = current
                .app
                .context()
                .service::<SessionPersistenceSvc>()
                .ok_or(HostError::NotFound)?;
            let stored = store
                .describe(target)
                .await
                .map_err(|message| HostError::Failed { message })?;
            if stored.is_none() {
                return Err(HostError::NotFound);
            }
        }
        let next = self.mount(&opening).await?;
        let session = next.session.clone();
        let old = live
            .replace(next)
            .expect("a live session was checked above");
        let previous = old.session.clone();
        retire(old).await;
        drop(live);

        self.watchers
            .lock()
            .expect("watchers poisoned")
            .retain(|watcher| {
                watcher
                    .send(HostEvent::SessionChanged {
                        session: session.clone(),
                        previous: Some(previous.clone()),
                    })
                    .is_ok()
            });
        Ok(HostReply::SessionChanged { session })
    }

    async fn set_effort(
        &self,
        addressed: &str,
        level: Option<ReasoningEffort>,
    ) -> Result<HostReply, HostError> {
        let mut live = self.live.lock().await;
        let current = live.as_mut().ok_or(HostError::Unavailable)?;
        if current.session != addressed {
            return Err(HostError::NotFound);
        }
        // The level is the session's, on the row that applies it to every
        // request and says so when the agent is described.
        let config = match level {
            Some(level) => format!("{{ level = {:?} }}", level.as_str()),
            None => "{}".to_string(),
        };
        let layer = Layer::from_toml(&format!(
            "[[patch]]\nid = \"reasoning-effort\"\nconfig = {config}\n"
        ))
        .map_err(|e| HostError::Failed {
            message: e.to_string(),
        })?;
        current
            .app
            .patch(&layer)
            .await
            .map_err(|e| HostError::Failed {
                message: e.to_string(),
            })?;
        Ok(HostReply::Done)
    }

    async fn list(&self, working_dir: Option<String>) -> Result<HostReply, HostError> {
        let ctx = match self.live.lock().await.as_ref() {
            Some(live) => live.app.context(),
            None => return Err(HostError::Unavailable),
        };
        let Some(store) = ctx.service::<SessionPersistenceSvc>() else {
            return Ok(HostReply::Sessions {
                sessions: Vec::new(),
            });
        };
        let failed = |message: String| HostError::Failed { message };
        let mut sessions = Vec::new();
        for id in store.list().await.map_err(failed)? {
            let Some(summary) = store.describe(&id).await.map_err(failed)? else {
                continue;
            };
            let header = summary.header;
            let dir = header.as_ref().and_then(|h| h.cwd.clone());
            if working_dir.is_some() && dir != working_dir {
                continue;
            }
            sessions.push(StoredSession {
                id,
                title: summary.title,
                working_dir: dir,
                created_at: header.map(|h| h.created_at).unwrap_or(0),
                updated_at: 0,
                turns: u32::try_from(summary.turns).unwrap_or(u32::MAX),
            });
        }
        Ok(HostReply::Sessions { sessions })
    }
}

/// Shut a replaced agent down and unmount its App. Its events are relayed to
/// the end first, so nothing it says arrives after the session that replaced it
/// is announced.
async fn retire(old: Live) {
    let Live {
        mut app,
        commands,
        relay,
        ..
    } = old;
    let _ = commands.send(AgentCommand::Shutdown);
    drop(commands);
    let _ = tokio::time::timeout(Duration::from_secs(10), relay).await;
    app.stop();
}

#[async_trait]
impl HostControl for TreeHost {
    async fn call(&self, command: HostCommand) -> Result<HostReply, HostError> {
        match command {
            HostCommand::NewSession { session } => self.replace(&session, Opening::Fresh).await,
            HostCommand::Resume { session, target } => {
                self.replace(&session, Opening::Resume(target)).await
            }
            HostCommand::SetReasoningEffort { session, level } => {
                self.set_effort(&session, level).await
            }
            HostCommand::ListSessions { working_dir } => self.list(working_dir).await,
            _ => Err(HostError::Failed {
                message: "this host does not do that".into(),
            }),
        }
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.watchers.lock().expect("watchers poisoned").push(tx);
        rx
    }
}
