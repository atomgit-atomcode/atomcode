//! The feed a front end is given: one end of the handle protocol, and the row
//! that pumps an App's facts into it.
//!
//! **Only the feed.** Host control — the contract a front end asks things of,
//! and the adapter that translates it onto `CodingRuntimeHandle` — used to live
//! here too, and that is what made this crate depend on a front-end contract.
//! It went to the host (`atomcode-cli`) on 2026-09-18, where
//! `docs/architecture-target.md` §2.4 says a host's own wiring belongs; this
//! crate is a Product (§2.3) and knows no front end.
//!
//! What the adapter needs of this type it takes through the accessors below,
//! which is why they are public: a bag of fields shared with code in the same
//! file is not a seam, and the moment that code moved out it had to become one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::feed::Feed;
use atomcode_kernel::event::AgentEvent;
use atomcode_plexus::{Context, Plugin};
use tokio::sync::mpsc;

/// What a runtime keeps for a front end across the Apps it builds.
pub struct FrontEnd {
    feed: Arc<Feed>,
    /// Messages this front end sent to team members, until each is claimed.
    forwarded: Arc<atomcode_harness::plugins::handle::Forwarded>,
    events: mpsc::UnboundedSender<AgentEvent>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
    /// The App now mounted, by the mount that set it. A rebuilt App mounts its
    /// row before the old one unloads, so an unload only clears its own.
    app: Mutex<Option<(u64, Context)>>,
    mounts: AtomicU64,
}

/// What a host knows about configuration that a front end asks it to act on:
/// the provider settings a model id means, and the settings as they are now
/// (`docs/adr/0021`, M5.4 addendum). The front end names the intent; the host
/// reads its own configuration.

impl FrontEnd {
    /// The end of the handle protocol the host hands to a front end. Taken
    /// once: a second caller would split one stream in two.
    pub fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>> {
        self.receiver.lock().expect("front end poisoned").take()
    }

    /// Where the host's adapter puts the events it translates.
    pub fn events(&self) -> mpsc::UnboundedSender<AgentEvent> {
        self.events.clone()
    }

    /// The feed this front end reads, for a host wiring a new App into it.
    pub fn feed(&self) -> Arc<Feed> {
        self.feed.clone()
    }

    /// Messages sent to team members that no member has claimed yet. The host's
    /// adapter hands them on when one arrives.
    pub fn forwarded(&self) -> Arc<atomcode_harness::plugins::handle::Forwarded> {
        self.forwarded.clone()
    }
}

impl std::fmt::Debug for FrontEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrontEnd")
    }
}

impl FrontEnd {
    pub fn new() -> Arc<Self> {
        let (events, receiver) = mpsc::unbounded_channel();
        Arc::new(Self {
            feed: Feed::new(events.clone()),
            forwarded: atomcode_harness::plugins::handle::Forwarded::new(),
            events,
            receiver: Mutex::new(Some(receiver)),
            app: Mutex::new(None),
            mounts: AtomicU64::new(0),
        })
    }

    /// How many Apps have fed this front end. A host that rebuilds its App for
    /// something done to the same session shows up here (`docs/adr/0022` §2).
    pub fn apps_fed(&self) -> u64 {
        self.mounts.load(Ordering::SeqCst)
    }

    /// The App now mounted, for the host's adapter.
    pub fn app(&self) -> Option<Context> {
        self.app
            .lock()
            .expect("front end poisoned")
            .as_ref()
            .map(|(_, ctx)| ctx.clone())
    }
}

/// `front-end-feed`: pushes this App's facts and agents to the front end the
/// runtime was started with.
pub struct FrontEndFeedPlugin(pub Arc<FrontEnd>);

#[async_trait]
impl Plugin for FrontEndFeedPlugin {
    fn name(&self) -> &'static str {
        "front-end-feed"
    }
    fn description(&self) -> &'static str {
        "session facts and agent status, pushed to a front end outside this App"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let front = self.0.clone();
        // Registered through this row's context, so they go when it unloads.
        let _ = front.feed.attach(ctx);
        let _ = front.forwarded.listen(ctx, front.events.clone());
        let mount = front.mounts.fetch_add(1, Ordering::SeqCst);
        *front.app.lock().expect("front end poisoned") = Some((mount, ctx.clone()));
        let _ = ctx.effect(move || {
            let mut app = front.app.lock().expect("front end poisoned");
            if app.as_ref().is_some_and(|(m, _)| *m == mount) {
                *app = None;
            }
        });
        Ok(())
    }
}
