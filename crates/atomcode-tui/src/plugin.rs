//! The rows: how this UI mounts into a config tree of its own.
//!
//! The screen is an App apart from the agent it drives (`docs/adr/0022` §3).
//! It holds the surface, the panels, the commands and the layout; the agent's
//! App belongs to whoever hosts it, and reaches the screen as a
//! [`HostConnection`] — the handle protocol and host control. When the host
//! replaces the session, the screen keeps its connection and starts drawing
//! the new session (`docs/adr/0022` §6).

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::seams::{UiSvc, UserInterface};
use atomcode_kernel::agent::{AgentDescription, AgentStatus};
use atomcode_kernel::event::{AgentCommand, AgentEvent, CommandId, RequestId};
use atomcode_kernel::host::{HostConnection, HostControl, HostEvent};
use atomcode_kernel::session::{Committed, LoggedEvent, SeqNo};
use atomcode_plexus::{plexus_service, Context, Plugin};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::block::Stream;
use crate::host::{default_layout, Host};
use crate::keymap::{Action, Keys};
use crate::module::Modules;
use crate::moment::Timestamp;
use crate::surface::{Headless, Input, Surface, Terminal};

plexus_service!(SurfaceSvc => dyn Surface, "surface", Seam, "Where a frame is painted");
plexus_service!(ModulesSvc => Modules, "tui-modules", Core, "Mounted stream producers and view modules");
// The region tree, as a service, because every panel that puts itself on screen
// when it mounts needs it. Before it was a field on `Host` reachable only from
// inside this file, which is precisely why the mascot had to be a special case.
plexus_service!(LayoutSvc => crate::layout::Layout, "tui-layout", Core, "The region tree on screen");
plexus_service!(CommandsSvc => crate::command::Commands, "tui-commands", Core, "Slash commands contributed by rows");
plexus_service!(AgentClientSvc => AgentClient, "tui-agent-client", Core, "The screen's end of its connection to the agent");
// Declared here, by the one that consumes it (`docs/adr/0021` §6): whoever
// launches the screen fills it with what its host handed over.
plexus_service!(ConnectionSvc => Connection, "agent-connection", Seam, "What the host handed this screen: its agent and host control");

/// The connection a launcher hands the screen, taken once when it runs.
pub struct Connection(Mutex<Option<HostConnection>>);

impl Connection {
    pub fn new(connection: HostConnection) -> Self {
        Self(Mutex::new(Some(connection)))
    }
    fn take(&self) -> Option<HostConnection> {
        self.0.lock().expect("connection poisoned").take()
    }
}
// Mounted cell-grid bitmaps. The host holds the table and puts a snapshot into
// every frame's `Moment`; a row reaches it here to mount and repaint. See
// `docs/adr/0027`.
plexus_service!(RastersSvc => crate::raster::Rasters, "tui-rasters", Core, "Cell-grid bitmaps, addressed by (module id, key)");

/// The session's clock, and the only place this crate reads one.
///
/// A duration on screen is the difference of two readings the log does not
/// carry, so the readings travel in `Moment` and every `render` stays a pure
/// function of them (`docs/adr/0008`). Read here because the host loop is the
/// only thing that knows when *now* is: it wakes for a fact, for a key and for a
/// tick, and each of those is a moment the screen is painted from.
struct Clock(std::time::Instant);

impl Clock {
    /// Start counting. Called before anything can commit a fact, so a turn that
    /// opens during start-up is measured on the same clock as every later one.
    fn start() -> Self {
        Self(std::time::Instant::now())
    }

    /// How long this session has been on screen.
    fn reading(&self) -> Timestamp {
        // Milliseconds since the loop started, saturated rather than wrapped: a
        // u64 of milliseconds is 584 million years, and a number that suddenly
        // went backwards would read as a turn that started in the future.
        let ms = u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX);
        Timestamp::millis(ms)
    }
}

/// The screen's end of its connection to the agent.
///
/// Everything this UI tells the agent goes through here as an
/// [`AgentCommand`], and everything the screen knows about the session it
/// draws — its facts, its description, whether it is busy — it learned from
/// what came back. It reads no service of the agent's own: the agent is in
/// another App, and possibly another process.
#[derive(Default)]
pub struct AgentClient {
    link: Mutex<Option<Link>>,
    view: Mutex<Views>,
    receipts: AtomicU64,
}

struct Link {
    commands: mpsc::UnboundedSender<AgentCommand>,
    control: Arc<dyn HostControl>,
}

/// What this screen follows: one session, and the members of its team the
/// person has looked at (`docs/adr/0023` §3).
#[derive(Default)]
struct Views {
    /// The session the host gave: the lead, when there is a team.
    root: String,
    /// The one on screen — the root, or a member of its team.
    on_screen: String,
    /// Each followed session's facts and description, kept while another is on
    /// screen: switching back draws what is here rather than asking again.
    sessions: std::collections::HashMap<String, SessionView>,
    /// The root's team, by session id, as members joined and left.
    members: std::collections::BTreeSet<String>,
}

/// One followed session, as its facts and events have described it.
#[derive(Default)]
struct SessionView {
    /// The last fact folded.
    high: Option<SeqNo>,
    events: Vec<LoggedEvent>,
    described: Option<AgentDescription>,
    status: Option<AgentStatus>,
    /// Messages sent and not yet taken by a turn.
    outstanding: HashSet<CommandId>,
}

impl Views {
    fn screen(&self) -> Option<&SessionView> {
        self.sessions.get(&self.on_screen)
    }

    /// `command`, for the agent on screen: as it is for the root, addressed to
    /// the member otherwise.
    fn addressed(&self, command: AgentCommand) -> AgentCommand {
        if self.on_screen == self.root {
            command
        } else {
            AgentCommand::To {
                session: self.on_screen.clone(),
                command: Box::new(command),
            }
        }
    }
}

impl AgentClient {
    pub(crate) fn connect(
        &self,
        commands: mpsc::UnboundedSender<AgentCommand>,
        control: Arc<dyn HostControl>,
    ) {
        *self.link.lock().expect("client poisoned") = Some(Link { commands, control });
    }

    fn command(&self, command: AgentCommand) {
        if let Some(link) = self.link.lock().expect("client poisoned").as_ref() {
            let _ = link.commands.send(command);
        }
    }

    /// The session on screen.
    pub fn session(&self) -> String {
        self.view.lock().expect("client poisoned").on_screen.clone()
    }

    /// The session this screen follows — the lead, when there is a team.
    pub fn root(&self) -> String {
        self.view.lock().expect("client poisoned").root.clone()
    }

    /// The last fact of the session this screen follows — what a host command
    /// about its conversation is based on (`docs/adr/0021` §9).
    pub fn root_high(&self) -> atomcode_kernel::session::SeqNo {
        let views = self.view.lock().expect("client poisoned");
        views
            .sessions
            .get(&views.root)
            .and_then(|view| view.high)
            .unwrap_or(0)
    }

    /// The facts of the session on screen so far, in log order.
    pub fn events(&self) -> Vec<LoggedEvent> {
        self.view
            .lock()
            .expect("client poisoned")
            .screen()
            .map(|v| v.events.clone())
            .unwrap_or_default()
    }

    /// What the agent on screen was last described as.
    pub fn described(&self) -> Option<AgentDescription> {
        self.view
            .lock()
            .expect("client poisoned")
            .screen()
            .and_then(|v| v.described.clone())
    }

    /// Host control, once connected.
    pub fn control(&self) -> Option<Arc<dyn HostControl>> {
        self.link
            .lock()
            .expect("client poisoned")
            .as_ref()
            .map(|link| link.control.clone())
    }

    /// Nothing sent to the agent on screen is still waiting for a turn, and it
    /// says it is idle.
    pub fn settled(&self) -> bool {
        let views = self.view.lock().expect("client poisoned");
        views.screen().is_none_or(|view| {
            view.outstanding.is_empty() && matches!(view.status, None | Some(AgentStatus::Idle))
        })
    }

    /// Say something to the agent on screen.
    pub fn send(&self, text: String, images: Vec<atomcode_kernel::message::ImageContent>) {
        let id = format!("tui-{}", self.receipts.fetch_add(1, Ordering::SeqCst));
        let command = {
            let mut views = self.view.lock().expect("client poisoned");
            let on_screen = views.on_screen.clone();
            if let Some(view) = views.sessions.get_mut(&on_screen) {
                view.outstanding.insert(id.clone());
            }
            views.addressed(AgentCommand::Tagged {
                id,
                command: Box::new(AgentCommand::SendMessage { text, images }),
            })
        };
        self.command(command);
    }
    /// Stop the turn of the agent on screen.
    pub fn cancel(&self) {
        let command = self
            .view
            .lock()
            .expect("client poisoned")
            .addressed(AgentCommand::Cancel);
        self.command(command);
    }
    /// Stop the turn of the session this screen follows and of every member of
    /// its team; the members stay (`docs/adr/0023` §9). Says how many members it
    /// asked.
    pub fn cancel_all(&self) -> usize {
        self.command(AgentCommand::Cancel);
        let members = self.view.lock().expect("client poisoned").members.clone();
        for session in &members {
            self.command(AgentCommand::To {
                session: session.clone(),
                command: Box::new(AgentCommand::Cancel),
            });
        }
        members.len()
    }
    /// Run a command from the catalog of the agent on screen.
    pub fn invoke(&self, name: &str, args: &str) {
        let id = format!("tui-{}", self.receipts.fetch_add(1, Ordering::SeqCst));
        let session = self.session();
        self.command(AgentCommand::Invoke {
            id,
            session,
            name: name.to_string(),
            args: args.to_string(),
        });
    }
    /// Compact the conversation of the agent on screen.
    pub fn compact(&self, focus: Option<String>) {
        let command = self
            .view
            .lock()
            .expect("client poisoned")
            .addressed(AgentCommand::Compact { focus });
        self.command(command);
    }
    pub fn shutdown(&self) {
        self.command(AgentCommand::Shutdown);
    }
    fn respond(&self, id: RequestId, value: Value) {
        self.command(AgentCommand::Respond { id, value });
    }

    /// Draw `session` from its first fact. Whatever was followed before — the
    /// session and any member looked at — is let go.
    pub(crate) fn follow(&self, session: &str) {
        let previous = {
            let mut views = self.view.lock().expect("client poisoned");
            let previous: Vec<String> = views.sessions.keys().cloned().collect();
            *views = Views {
                root: session.to_string(),
                on_screen: session.to_string(),
                ..Views::default()
            };
            views
                .sessions
                .insert(session.to_string(), SessionView::default());
            previous
        };
        for old in previous.into_iter().filter(|old| old != session) {
            self.command(AgentCommand::Unsubscribe { session: old });
        }
        self.command(AgentCommand::Subscribe {
            session: session.to_string(),
            from: 0,
        });
    }

    /// Put `session` — the root or a member of its team — on screen. `None` when
    /// it already is; otherwise what is known of it so far, to draw from. A
    /// member looked at for the first time is followed from its first fact, and
    /// stays followed: looking at it again draws what arrived meanwhile.
    pub(crate) fn look_at(&self, session: &str) -> Option<Vec<LoggedEvent>> {
        let (known, subscribe) = {
            let mut views = self.view.lock().expect("client poisoned");
            if views.on_screen == session {
                return None;
            }
            views.on_screen = session.to_string();
            match views.sessions.get(session) {
                Some(view) => (view.events.clone(), false),
                None => {
                    views
                        .sessions
                        .insert(session.to_string(), SessionView::default());
                    (Vec::new(), true)
                }
            }
        };
        if subscribe {
            self.command(AgentCommand::Subscribe {
                session: session.to_string(),
                from: 0,
            });
        }
        Some(known)
    }

    /// Keep a fact of a followed session. `true` when it is the one on screen
    /// and new, which is when it is drawn.
    pub(crate) fn keep(&self, committed: &Committed) -> bool {
        let mut views = self.view.lock().expect("client poisoned");
        let on_screen = views.on_screen == committed.session;
        let Some(view) = views.sessions.get_mut(&committed.session) else {
            return false;
        };
        if view.high.is_some_and(|h| committed.seq <= h) {
            return false;
        }
        view.high = Some(committed.seq);
        view.events.push(LoggedEvent {
            seq: committed.seq,
            at: committed.at,
            event: committed.event.clone(),
        });
        on_screen
    }

    fn describe(&self, description: &AgentDescription) {
        let mut views = self.view.lock().expect("client poisoned");
        if let Some(view) = views.sessions.get_mut(&description.session) {
            view.described = Some(description.clone());
        }
    }

    /// A member of the root joined, or left.
    fn member(&self, session: &str, joined: bool) {
        let mut views = self.view.lock().expect("client poisoned");
        if joined {
            views.members.insert(session.to_string());
        } else {
            views.members.remove(session);
        }
    }

    /// `true` when it is the session on screen.
    fn status(&self, session: &str, status: AgentStatus) -> bool {
        let mut views = self.view.lock().expect("client poisoned");
        if let Some(view) = views.sessions.get_mut(session) {
            view.status = Some(status);
        }
        views.on_screen == session
    }

    /// The level host control accepted, onto the description the screen holds
    /// until the agent is described again.
    pub(crate) fn chose_effort(&self, level: Option<atomcode_kernel::provider::ReasoningEffort>) {
        let mut views = self.view.lock().expect("client poisoned");
        let on_screen = views.on_screen.clone();
        if let Some(described) = views
            .sessions
            .get_mut(&on_screen)
            .and_then(|v| v.described.as_mut())
        {
            described.reasoning_effort = level;
        }
    }

    fn answered(&self, receipt: &str) {
        for view in self
            .view
            .lock()
            .expect("client poisoned")
            .sessions
            .values_mut()
        {
            view.outstanding.remove(receipt);
        }
    }
}

/// Lines the conversation moves per wheel notch.
///
/// One, not three. The terminal already sends one event per notch, so three
/// lines an event is three times the distance the hand asked for — and the
/// reason to reach for the wheel here is to study something that went past,
/// which is exactly when precision beats speed.
const WHEEL_LINES: i32 = 1;

/// The ways out of a policy intervention, as a person reads them.
///
/// The intervention's own list, in its order: the kernel says which apply, and
/// offering one it did not name would be offering something that will be
/// refused. The words are the same ones the `policy` command takes (M5.5), so
/// what a person picks is what gets run.
fn policy_options(
    intervention: &atomcode_kernel::event::PolicyIntervention,
) -> Vec<atomcode_harness::seams::Answer> {
    use atomcode_kernel::event::PolicyRecoveryAction as A;
    intervention
        .actions
        .iter()
        .filter_map(|action| match action {
            A::CompleteExternally => Some(("done", "我自己在外面做完")),
            A::SkipStep => Some(("skip", "跳过这一步")),
            A::ViewSafeInstructions => Some(("how", "看看安全的做法")),
            A::EndTask => Some(("end", "到此为止")),
            // A way out added since this screen was written: left out rather
            // than guessed at — an unlabelled row is one nobody can choose on
            // purpose.
            _ => None,
        })
        .map(|(value, label)| {
            atomcode_harness::seams::Answer::labelled(value.to_string(), label.to_string())
        })
        .collect()
}

/// Answer the password prompts `sudo` and `ssh` make, for as long as the screen
/// is up.
///
/// `sudo` and `ssh` read a password from the tty — and this screen owns the tty,
/// so a `sudo` inside a tool call waits forever for keys it will never get. Both
/// programs prefer an `*_ASKPASS` helper when one is set; the `bash` tool sets
/// those for every child it spawns, and this is the other end: the server they
/// reach, and a modal that asks the person.
///
/// What comes back is returned rather than dropped: the guard removes the socket,
/// and the pump ends with it. A screen that has gone away must not keep a socket
/// that promises an answer.
#[cfg(unix)]
fn serve_askpass(
    host: Arc<crate::host::Host>,
    wake: mpsc::UnboundedSender<Wake>,
) -> Option<atomcode_capabilities::askpass::server::AskpassServerGuard> {
    use atomcode_capabilities::askpass;

    // Five minutes, as the cache was built for: long enough that a `sudo` per
    // tool call in one stretch of work asks once, short enough that a screen
    // left open overnight does not still hold it.
    let cache = Arc::new(askpass::cache::PasswordCache::new(
        std::time::Duration::from_secs(300),
    ));
    let (mut env, prompts, guard) = askpass::server::start(cache).ok()?;
    // Without the wrapper script there is nothing for sudo to exec, so the env
    // is left unset and the whole thing degrades to what it is today: no
    // askpass. Degrading beats a half-set environment that sends sudo to a
    // script that is not there.
    let script = std::env::current_exe().ok().and_then(|exe| {
        env.sock_path
            .parent()
            .and_then(|dir| askpass::wrapper::write_askpass_script(&exe, dir).ok())
    })?;
    env.askpass_script = script;
    askpass::set_env(env);
    tokio::spawn(answer_prompts(host, wake, prompts));
    Some(guard)
}

/// Put each prompt on screen and hand back what the person typed.
///
/// Split from [`serve_askpass`] so it can be judged without a socket: what is
/// worth judging is that a prompt becomes a modal, that the answer reaches the
/// one waiting for it, and that esc reaches them as a refusal.
#[cfg(unix)]
async fn answer_prompts(
    host: Arc<crate::host::Host>,
    wake: mpsc::UnboundedSender<Wake>,
    mut prompts: tokio::sync::mpsc::Receiver<atomcode_capabilities::askpass::server::AskpassPrompt>,
) {
    while let Some(prompt) = prompts.recv().await {
        let asking = Arc::new(crate::secret::SecretPrompt::new(prompt.prompt));
        let reply = Mutex::new(Some(prompt.reply));
        host.overlays.open(
            asking,
            Box::new(move |answer| {
                if let Some(reply) = reply.lock().expect("askpass reply poisoned").take() {
                    // `None` is a refusal — the person pressed esc — and every
                    // reader downstream must take it as one.
                    let _ = reply.send(answer);
                }
            }),
        );
        // The prompt arrives while nothing else is waking the loop: a turn is
        // running and the screen is idle between frames.
        if wake.send(Wake::Fact).is_err() {
            break;
        }
    }
}

/// How many wakes are answered before a frame is owed regardless.
///
/// The loop paints when the queue has run dry, so a burst of wakes — a turn
/// committing a fact per token, and the handle saying a second thing about each
/// one — is one repaint instead of one per token. This is what stops a producer
/// that refills the queue as fast as the loop drains it from holding the screen
/// without ever showing it: coalescing is a reason not to redraw each wake, not
/// a reason never to redraw.
const COALESCE_LIMIT: usize = 256;

/// What woke the loop up.
enum Wake {
    Fact,
    /// Something came back over the connection: a fact, a turn boundary, a
    /// question, what the agents are.
    Event(AgentEvent),
    /// The host replaced the session.
    Host(HostEvent),
    Input(Input),
    /// An action from somewhere other than a key — a command, for now.
    Act(Action),
    /// A modal closed with this.
    Chose(Option<String>),
    Tick,
    Closed,
}

/// This row has no config left. The one knob it had — `mascot = true` — is now
/// `[[insert]] name = "tui-panel-mascot"`, which is the point: a panel is a row,
/// not a boolean on somebody else's row.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {}

/// A member of the session on screen, as its events have told it.
struct Member {
    name: String,
    status: AgentStatus,
    /// Turns it has opened while watched.
    turns: u64,
    /// Stopped and gone from the registry: kept, so it can still be looked at.
    gone: bool,
}

/// The assembled UI. Public so a test can drive exactly what ships.
pub struct Tui {
    client: Arc<AgentClient>,
    host: Arc<Host>,
    keys: Keys,
    surface: Arc<dyn Surface>,
    /// Set when the loop starts. A command runs against the tree, and the tree
    /// is not known until then.
    ctx: Mutex<Option<Context>>,
    wake: Mutex<Option<mpsc::UnboundedSender<Wake>>>,
    /// Where the button went down, so a release can tell a click from a drag.
    pressed_at: Mutex<Option<(u16, u16)>>,
    /// The session's members, by session id.
    members: Mutex<BTreeMap<String, Member>>,
}

#[async_trait]
impl UserInterface for Tui {
    fn describe(&self) -> String {
        format!("full-screen terminal on {}", self.surface.describe())
    }

    async fn run(&self, ctx: &Context, initial: Option<String>) -> Result<(), String> {
        let HostConnection {
            session,
            commands,
            events: mut agent_events,
            control,
        } = ctx
            .service::<ConnectionSvc>()
            .and_then(|slot| slot.take())
            .ok_or("no connection to an agent: the launcher provides `agent-connection`")?;
        let client = self.client.clone();
        let mut host_events = control.subscribe();
        client.connect(commands, control);

        let (wake_tx, mut wake) = mpsc::unbounded_channel::<Wake>();
        // Started before anything can commit a fact, so the first turn's
        // opening reading is measured on the same clock as every later one.
        let clock = Clock::start();
        {
            // Where we are is environment, not a fact from the log — which is
            // what `Moment` is for. Read once here rather than per frame: a
            // `render` that called `current_dir()` would be doing I/O in a
            // function whose whole contract is purity.
            let mut m = self.host.moment.write().expect("moment poisoned");
            m.cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            // What the terminal can show, from the one row allowed to ask.
            // Without this every module renders against `Caps::default()` — a
            // dark, fully capable terminal — whatever the real one is, which is
            // how a light terminal ended up painted in a dark palette.
            m.caps = self.surface.caps();
            m.now = clock.reading();
        }
        *self.ctx.lock().expect("ctx poisoned") = Some(ctx.clone());
        *self.wake.lock().expect("wake poisoned") = Some(wake_tx.clone());

        // A question can arrive mid-turn, when nothing else is waking the loop.
        let (ask_tx, mut ask_rx) = mpsc::unbounded_channel::<()>();
        self.host.asks.notify_on(ask_tx);
        let asked = wake_tx.clone();
        let asks_pump = tokio::spawn(async move {
            while ask_rx.recv().await.is_some() {
                if asked.send(Wake::Fact).is_err() {
                    break;
                }
            }
        });

        // Everything the connection says. Content arrives as the session's facts
        // and is folded; the rest moves the status line, the members and the
        // questions.
        let said = wake_tx.clone();
        let events_pump = tokio::spawn(async move {
            while let Some(event) = agent_events.recv().await {
                if said.send(Wake::Event(event)).is_err() {
                    break;
                }
            }
            let _ = said.send(Wake::Closed);
        });
        let replaced = wake_tx.clone();
        let host_pump = tokio::spawn(async move {
            while let Some(event) = host_events.recv().await {
                if replaced.send(Wake::Host(event)).is_err() {
                    break;
                }
            }
        });

        // A password `sudo` or `ssh` wants, asked of the person instead of the
        // tty this screen owns (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md`
        // P0-1). Held to the end of this function: dropping the guard removes
        // the socket, so a screen that is gone stops answering.
        #[cfg(unix)]
        let _askpass = serve_askpass(self.host.clone(), wake_tx.clone());

        // The session on screen, from its first fact: a resumed session and a
        // live one produce the same picture, because the history is facts too.
        client.follow(&session);
        {
            let mut m = self.host.moment.write().expect("moment poisoned");
            m.lead = session.clone();
            m.viewing = session.clone();
        }

        // Whether the conversation still owes its first word. Answered in the
        // loop below rather than here, and the reason is the whole of it:
        // `open_conversation` opens **only when the stream is empty**, and a
        // resumed session's history no longer arrives before this point. It comes
        // as facts over the subscription just sent (`follow` above), which means
        // that asking now would find every session empty and put a welcome block
        // in front of every resumed conversation.
        //
        // So the question is asked at the first moment the answer means anything:
        // the loop about to paint with nothing left in the queue. The ordering
        // that makes that sound is the feed's (`harness/src/feed.rs`): on
        // `Subscribe` it sends `Described`, then the status, the members, and
        // then every fact from `from` on — so a description in hand plus a
        // drained queue is exactly "the history, if any, is already folded".
        let mut owes_opening = true;

        // Input comes from the surface when it has its own — that is what a
        // headless run is — and from the terminal otherwise.
        let keys_tx = wake_tx.clone();
        let reader = match self.surface.take_input() {
            Some(mut scripted) => tokio::spawn(async move {
                while let Some(input) = scripted.recv().await {
                    if keys_tx.send(Wake::Input(input)).is_err() {
                        break;
                    }
                }
            }),
            None => tokio::spawn(async move { read_input(keys_tx).await }),
        };

        if let Some(text) = initial {
            // A start-up prompt is text by construction — there is no composer
            // yet, so there is nothing it could have been attached to.
            client.send(text, Vec::new());
        }

        let mut quit = false;
        // Whether what is on screen is out of date. `compose` walks the stream
        // and encodes a whole screen, so a frame is composed when the loop is
        // about to go idle on it rather than on the way past: a wake that
        // changed nothing must not cost one. The first is owed — nothing has
        // been drawn yet.
        let mut stale = true;
        // Wakes answered since that frame. See [`COALESCE_LIMIT`].
        let mut coalesced = 0usize;
        while !quit {
            // The composer's menu is the one thing here that follows the
            // pointer, so free motion is on exactly while it is up: with it on,
            // the terminal reports every cell the pointer crosses, and with
            // nothing following the pointer that is a packet per cell for
            // nothing. Asked here, at the top of every iteration, rather than
            // at each place the menu opens and closes — there are five of those
            // and one question, and a question asked in one place cannot
            // disagree with the answer. It has to be here rather than at the
            // foot of the loop because the pointer paths `continue` past
            // anything down there.
            //
            // **Two** things follow the pointer, and either one is a reason for
            // the terminal to report every cell it crosses: the composer's menu,
            // and a question on screen — where the row under the pointer is the
            // row a click would take, so a panel that did not follow would point
            // at its first answer while the hand is on its third.
            //
            // This is a request to the terminal, not a redraw: it changes what
            // the terminal *sends*, not what is on the screen, which is why it
            // is not folded into `stale`.
            self.surface.set_motion(
                self.host.context_menu_open()
                    || self.host.asks.is_waiting()
                    || self.team_on_screen(),
            );
            // The conversation's first word, asked for here and once.
            //
            // Ahead of the paint below rather than inside it: an opening is the
            // *reason* for a frame, not something to add to one already owed. A
            // new session has no facts, so nothing else would mark the screen
            // stale and the welcome would wait for a frame that never comes.
            //
            // Only with the queue actually drained: `described()` says the
            // subscription has been answered, and an empty queue says its
            // backfill — a resumed session's whole history — is already folded.
            {
                if owes_opening && wake.is_empty() {
                    if let Some(described) = client.described() {
                        let cwd = self
                            .host
                            .moment
                            .read()
                            .expect("moment poisoned")
                            .cwd
                            .clone();
                        let open = crate::module::Opening {
                            // Folded upstream: reading the environment is this
                            // function's business, and a module may not.
                            cwd: crate::text::collapse_home(&cwd),
                            // From the agent's own description, not from a
                            // service of the agent: this screen is a separate App
                            // (`docs/adr/0022`), and reading into the agent's tree
                            // is what `tests/guards.rs`
                            // (`the_screen_reads_no_service_of_the_agents`)
                            // forbids.
                            model: described.model.clone(),
                            version: env!("CARGO_PKG_VERSION"),
                            commands: self.host.commands.all(),
                        };
                        stale |= self
                            .host
                            .open_conversation(crate::block::Coord::default(), &open);
                        // Asked once, whatever the answer: a stream that was not
                        // empty will not become empty again, and one that opened
                        // is no longer empty.
                        owes_opening = false;
                    }
                }
            }
            if stale && (coalesced >= COALESCE_LIMIT || wake.is_empty()) {
                // The reading the frame about to be painted is drawn from. Facts
                // absorb against whatever the last one was — a frame at most out
                // of date, and the only reading available between commits.
                self.host.moment.write().expect("moment poisoned").now = clock.reading();
                self.refresh_members();
                // A question that arrived while the loop was asleep is brought
                // onto the moment here, before the frame it appears in is
                // composed. The question panel renders from the moment, and the
                // queue is the truth about whether one is waiting, so this is
                // where the two are reconciled — in one place, before any frame
                // can be drawn from a disagreement.
                self.host.sync_asking();
                self.paint();
                stale = false;
                coalesced = 0;
            }
            let timer = self.host.modules.tick();
            let woke = match timer {
                Some(every) => match tokio::time::timeout(every, wake.recv()).await {
                    Ok(Some(w)) => w,
                    Ok(None) => Wake::Closed,
                    Err(_) => Wake::Tick,
                },
                None => wake.recv().await.unwrap_or(Wake::Closed),
            };
            coalesced += 1;
            // The pointer's own vocabulary, needed in the patterns below rather
            // than only inside an arm body.
            use crate::surface::Click;
            // Say the mouse mode again on the wakes that can follow a terminal
            // putting its own tracker back without telling us — every tick, and
            // every keystroke. Idempotent by construction (a DECSET for a mode
            // already set changes nothing), so the cost is bytes and no state.
            //
            // Deliberately *not* on mouse events, which is the tempting case to
            // include and the one that would cost the most: an arriving mouse
            // event is itself proof that the tracker is on, so repeating the
            // mode there buys nothing. While the menu is up it is worse than
            // nothing — free motion reports every cell the pointer crosses, so
            // healing on each of those is exactly the packet-per-cell price
            // `ansi::MOUSE_MOTION_ON` exists to avoid, paid for information the
            // event already carries. The one mouse event worth acting on is the
            // one that should not have arrived at all; see the `Hover` arm below.
            //
            // Here rather than at the top of the loop because `woke` is what
            // says which of these it was, and before the `match` because the
            // pointer arms `continue` past anything below.
            if matches!(&woke, Wake::Tick | Wake::Input(Input::Key(_))) {
                self.surface.heal_mouse();
            }
            match woke {
                Wake::Closed => quit = true,
                // The fact was folded into the stream by the listener that sent
                // this, so what is drawn is a frame behind it now.
                Wake::Fact => stale = true,
                // Most of what arrives here moves nothing on the screen: a
                // streamed delta is already in the transcript by the time it is
                // projected into this event, and this is the same news again.
                // `on_event` says which arrivals did move it.
                Wake::Event(event) => stale |= self.on_event(event),
                // The host put another session in place of this one: draw that
                // one, from its first fact, in a stream of its own.
                Wake::Host(HostEvent::SessionChanged { session, .. }) => {
                    if session != client.root() {
                        self.members.lock().expect("members poisoned").clear();
                        self.host.switch_session();
                        client.follow(&session);
                        {
                            let mut m = self.host.moment.write().expect("moment poisoned");
                            m.lead = session.clone();
                            m.viewing = session.clone();
                        }
                        self.host.say(format!("已切换到会话 {session}"), false);
                    }
                    stale = true;
                }
                Wake::Host(_) => {}
                Wake::Act(action) => {
                    quit = self.act(action, &client);
                    stale = true;
                }
                Wake::Chose(chosen) => {
                    self.chose(chosen);
                    stale = true;
                }
                Wake::Tick => {
                    let mut m = self.host.moment.write().expect("moment poisoned");
                    m.tick = m.tick.wrapping_add(1);
                    stale = true;
                }
                // A move is the one event the tracker sends for a reason we did
                // not ask for: `Moved` arrives only while free motion is
                // reporting, and free motion is asked for exactly while the
                // menu is up. Seeing one with the menu down is the one piece of
                // evidence available that the terminal's tracker is not where
                // this side left it — which is what a session restore, a tab
                // switch or a stray reset from anything else holding the tty
                // does to it. Say the mode again, and say so, because the
                // pointer was in the terminal's hands until the next click
                // brought it back.
                //
                // Not a query: see `Surface::heal_mouse` for why asking is not
                // an option here.
                // A hover-arrival is proof the mode is *on* only for the one case
                // this arm was written for: free motion was not requested, so the
                // terminal must be reporting it on its own.
                //
                // With a question on screen we have just asked for it (see
                // `set_motion` below, and that is what starts this arm's one
                // firing), so an arriving hover is exactly what was requested —
                // treating it as a terminal that took the mouse back would say so
                // on the tip row and, worse, hand the pointer back the next time
                // the question is answered. The question's own hover is a
                // request, same as the menu's.
                Wake::Input(Input::Mouse(Click::Hover, ..))
                    if !self.host.context_menu_open()
                        && !self.host.asks.is_waiting()
                        && !self.team_on_screen() =>
                {
                    self.surface.heal_mouse();
                    self.host.say(
                        "鼠标被终端收回了,已自动要回;若再次发生,ctrl-o 可手动切换",
                        false,
                    );
                    stale = true;
                }
                Wake::Input(Input::Resize(..)) => {
                    // A resize is the terminal reflowing its own screen under
                    // us, and the repaint diff skips a row whose bytes did not
                    // change — so a screen the terminal rewrapped would stay
                    // rewrapped wherever our own content did not move. This is
                    // the one moment we *know* the cache is describing a screen
                    // that no longer exists; the size comparison in
                    // `Rows::patch_from` only catches the case where the size
                    // the terminal reports has already changed, and the event
                    // can arrive before it has. `ctrl-l` is the other way to
                    // say the same thing.
                    self.surface.forget();
                    stale = true;
                }
                // The wheel scrolls the conversation, not the terminal's own
                // history — in the alternate screen that history is the shell's,
                // so a wheel the terminal keeps would scroll the wrong thing.
                Wake::Input(Input::Mouse(click, x, y)) => {
                    use crate::surface::Click;
                    // The composer's menu, before anything else looks at the
                    // pointer. A menu that is up takes the press: choosing from
                    // it is what a press means while it is there. A press
                    // anywhere else puts it away *and* still means what it
                    // meant, which is why "dismissed" falls through rather than
                    // swallowing the event.
                    if let Some(handled) = self.pointer_in_menu(click, x, y, &client) {
                        quit = handled;
                        stale = true;
                        continue;
                    }
                    // A press is not yet a click and not yet a selection —
                    // which it becomes is decided at the release, by whether
                    // the pointer moved. Deciding at the press would mean
                    // folding a block every time someone selects text on it.
                    let action = match click {
                        Click::WheelUp => Some(Action::Scroll(-WHEEL_LINES)),
                        Click::WheelDown => Some(Action::Scroll(WHEEL_LINES)),
                        Click::Press => {
                            *self.pressed_at.lock().expect("press poisoned") = Some((x, y));
                            // A press on an answer is the answer. It is not the
                            // start of a text selection and not a fold: the
                            // panel is a choice, and waiting for the release
                            // would let a press-and-drag over one answer land on
                            // another one's row.
                            if self.host.asks.is_waiting() {
                                if let Some(row) = self.host.answer_row_at(x, y) {
                                    let _ = self.host.point_ask_at(row);
                                    quit = self.confirm_question();
                                    stale = true;
                                    continue;
                                }
                            }
                            // A press on a team row is a switch to that agent.
                            if let Some(row) = self.host.team_row_at(x, y) {
                                let _ = self.host.point_team_at(row);
                                if let Some(session) = self.host.unfocus_team() {
                                    self.switch_to(&session);
                                }
                                stale = true;
                                continue;
                            }
                            Some(Action::SelectFrom(x, y))
                        }
                        Click::Drag => Some(Action::SelectTo(x, y)),
                        Click::Release => {
                            let from = self.pressed_at.lock().expect("press poisoned").take();
                            match from {
                                Some(p) if p == (x, y) => Some(Action::ClickAt(x, y)),
                                Some(_) => Some(Action::CopySelection),
                                None => None,
                            }
                        }
                        // A move is not a press: it is the one pointer event
                        // that asks for nothing. The menu is the only thing
                        // here that follows a pointer, so it is asked; a move
                        // that changes no row paints nothing, because the
                        // terminal sends one per cell and a frame each would
                        // make the highlight cost more than it is worth.
                        //
                        // Asked only when a menu is up, because this arrives for
                        // every cell the pointer crosses and the size is an
                        // ioctl on a real terminal: with nothing open there is
                        // nothing to ask, and the pointer moves all day.
                        Click::Hover => {
                            if self.host.context_menu_open() {
                                stale |= self.host.context_menu_hover(x, y, self.surface.size());
                            }
                            // A question is the other thing on screen that follows
                            // a pointer, and for the same reason: the row under
                            // the pointer is the row a click would take, and a
                            // highlight somewhere else while the pointer is
                            // somewhere is the panel lying about its own state.
                            //
                            // Free when no question is up — `asking` is a field
                            // read, not a size ioctl.
                            if self.host.asks.is_waiting() {
                                if let Some(row) = self.host.answer_row_at(x, y) {
                                    stale |= self.host.point_ask_at(row);
                                }
                            }
                            // The team panel lights the row under the pointer:
                            // the row a press would take.
                            if let Some(row) = self.host.team_row_at(x, y) {
                                stale |= self.host.point_team_at(row);
                            }
                            continue;
                        }
                        // Handled above, and never reached.
                        Click::RightPress => None,
                    };
                    if let Some(action) = action {
                        quit = self.act(action, &client);
                    }
                    stale = true;
                }
                Wake::Input(Input::Paste(text)) => {
                    quit = self.act(Action::Paste(text), &client);
                    stale = true;
                }
                // A modal has the keyboard while it is open, then the
                // question, then the ordinary bindings. Exactly one owner at a
                // time, decided here — that is what focus is.
                Wake::Input(Input::Key(press)) if self.host.overlays.is_open() => {
                    self.host.overlays.key(press);
                    stale = true;
                }
                // The composer's menu, above the question and the ordinary
                // bindings for the same reason the modal is: it was opened
                // deliberately, and what it is for is being read right now.
                Wake::Input(Input::Key(press)) if self.host.context_menu_open() => {
                    if let Some(crate::menu::Step::Picked(value)) =
                        self.host.context_menu_key(press)
                    {
                        quit = self.run_menu_item(&value, &client);
                    }
                    stale = true;
                }
                // A question on screen gets first refusal on every key. It is a
                // panel riding the tail now, not a modal, so this is the only
                // place its keys are routed — and focus is still arbitration,
                // not composition: exactly one thing can hold it.
                Wake::Input(Input::Key(press)) if self.host.asks.is_waiting() => {
                    quit = self.answer_question(press);
                    stale = true;
                }
                // The team panel, once Tab gave it the keyboard.
                Wake::Input(Input::Key(press)) if self.host.team_focused() => {
                    quit = self.team_key(press);
                    stale = true;
                }
                Wake::Input(Input::Key(press))
                    if matches!(press.key, crate::surface::Key::Tab)
                        && press.mods == crate::surface::Mods::NONE
                        && self.host.focus_team() =>
                {
                    stale = true;
                }
                Wake::Input(Input::Key(press)) => {
                    if let Some(action) = self.keys.resolve(press) {
                        quit = self.act(action, &client);
                    }
                    stale = true;
                }
            }
        }

        reader.abort();
        asks_pump.abort();
        host_pump.abort();
        // Refuse what is waiting and stop what is running, then wait for the
        // turn to say it has ended — but not forever: a tool that ignores its
        // cancel is not a reason to leave the terminal in the alternate screen.
        self.host.asks.refuse_all();
        if !client.settled() {
            client.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while let Some(woke) = wake.recv().await {
                    if matches!(
                        woke,
                        Wake::Closed
                            | Wake::Event(AgentEvent::TurnComplete { .. } | AgentEvent::Cancelled)
                    ) {
                        break;
                    }
                }
            })
            .await;
        }
        client.shutdown();
        events_pump.abort();
        self.host.overlays.close_all();
        self.surface.restore();
        // Full screen swallows the conversation on exit; hand it back so a
        // person can still scroll up and see what happened.
        self.dump();
        Ok(())
    }
}

impl Tui {
    fn paint(&self) {
        let frame = self.host.compose(self.surface.size());
        self.surface.present(&frame);
    }

    /// The session's members, as their events have told them, onto the moment.
    ///
    /// Nothing is folded for a member: a member is created, opens a turn and
    /// is stopped without this conversation committing a single fact, and it
    /// must stay that way — a member's log is its own. What the screen knows of
    /// one is what the connection pushed about it.
    fn refresh_members(&self) {
        use crate::moment::{Activity, MemberNow};
        let mut members: Vec<MemberNow> = self
            .members
            .lock()
            .expect("members poisoned")
            .iter()
            .map(|(session, member)| MemberNow {
                name: member.name.clone(),
                activity: match member.status {
                    AgentStatus::Idle => Activity::Idle,
                    AgentStatus::Working => Activity::Working,
                    AgentStatus::Stopping => Activity::Stopping,
                },
                turn: member.turns,
                session: session.clone(),
                gone: member.gone,
            })
            .collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        let mut moment = self.host.moment.write().expect("moment poisoned");
        if moment.members != members {
            moment.members = members;
        }
    }

    /// Whether the team panel is drawn with rows to switch between: then the
    /// pointer is followed, for the row under it to light.
    fn team_on_screen(&self) -> bool {
        self.host.modules.has_view(crate::modules::team::ID)
            && !crate::modules::team::targets(&self.host.moment.read().expect("moment poisoned"))
                .is_empty()
    }

    /// Put `session` — the lead or a member — on screen (`docs/adr/0023` §3):
    /// the screen emptied and drawn again from what is known of it, the input
    /// and the stream now its. `false` when it was already there.
    fn look_at(&self, session: &str) -> bool {
        let Some(known) = self.client.look_at(session) else {
            return false;
        };
        self.host.switch_view();
        for logged in &known {
            self.host.absorb_logged(logged);
        }
        self.mark_undone();
        let working = self
            .members
            .lock()
            .expect("members poisoned")
            .get(session)
            .is_some_and(|m| m.status != AgentStatus::Idle && !m.gone);
        let mut m = self.host.moment.write().expect("moment poisoned");
        m.viewing = session.to_string();
        if working {
            m.activity = crate::moment::Activity::Working;
        }
        true
    }

    /// Tell the screen which turns were taken back, from the facts it has.
    fn mark_undone(&self) -> bool {
        self.host
            .mark_undone(atomcode_kernel::session::undone_turns(
                &self.client.events(),
            ))
    }

    /// A key while the team panel has the keyboard: move, switch, or give it back.
    fn team_key(&self, press: crate::surface::KeyPress) -> bool {
        use crate::surface::{Key, Mods};
        match (press.key, press.mods) {
            (Key::Up, _) | (Key::Char('k'), Mods::CTRL) => {
                self.host.move_team_by(-1);
            }
            (Key::Down, _) | (Key::Char('j'), Mods::CTRL) => {
                self.host.move_team_by(1);
            }
            (Key::Enter, _) => {
                if let Some(session) = self.host.unfocus_team() {
                    self.switch_to(&session);
                }
            }
            (Key::Esc, _) | (Key::Tab, _) | (Key::BackTab, _) => {
                let _ = self.host.unfocus_team();
            }
            (Key::Char('d'), Mods::CTRL) => return true,
            _ => {}
        }
        false
    }

    /// Switch the screen, and say where it went.
    fn switch_to(&self, session: &str) {
        if self.look_at(session) {
            let name = if session == self.client.root() {
                "主".to_string()
            } else {
                session.rsplit('/').next().unwrap_or(session).to_string()
            };
            self.host.say(format!("正在看 {name}"), false);
        }
    }

    /// A question from the agent, put on the screen and answered back.
    ///
    /// The question drawn is the one the agent wrote into the log just before
    /// asking, when it is there — options, asker and call exactly as recorded —
    /// and one read off the request otherwise. The answer goes back under the
    /// request's id, in the request's own terms.
    fn ask(&self, id: RequestId, kind: &str, payload: Value) {
        // A batch: several questions in one request
        // (`{"questions": [...]}` → `{"responses": [...]}`). Asked one at a
        // time, because a person answers one thing at a time, and answered as
        // one reply because that is what the asking tool waits for.
        //
        // Without this the screen could not parse the payload at all and
        // answered `Null` — which the tool reads as "no driver can present
        // this" and tells the model **interactive questions are not supported
        // in this environment**. A screen that is sitting right there, with a
        // question panel, saying it cannot ask.
        if let Some(questions) = crate::ask::batch_for(kind, &payload, &self.client.events()) {
            let asks = self.host.asks.clone();
            let client = self.client.clone();
            tokio::spawn(async move {
                let mut answers = Vec::with_capacity(questions.len());
                for question in questions {
                    // One at a time, in the order they were asked. A refusal
                    // answers *that* question and goes on to the next: the
                    // batch is several decisions, and declining one is not
                    // declining the rest.
                    let answered = asks.push(question.clone()).await.ok().flatten();
                    answers.push(crate::ask::declinable(&question, answered));
                }
                client.respond(id, serde_json::json!({ "responses": answers }));
            });
            return;
        }
        let Some(question) = crate::ask::question_for(kind, &payload, &self.client.events()) else {
            // Nothing this screen knows how to put to a person: refused, never
            // left hanging.
            self.client.respond(id, Value::Null);
            return;
        };
        let answer = self.host.asks.push(question.clone());
        let client = self.client.clone();
        let kind = kind.to_string();
        tokio::spawn(async move {
            let chosen = answer.await.ok().flatten();
            client.respond(id, crate::ask::response_for(&kind, &question, chosen));
        });
    }

    /// Put a policy intervention to the person, and act on what they pick.
    ///
    /// The ways out are the intervention's own — the kernel says which apply,
    /// and offering one it did not name would be offering something that will
    /// be refused. The choice is carried out by the `policy` command in the
    /// catalog (M5.5), which is where the two judgements about it live: whether
    /// anything is waiting, and whether this is one of its ways out
    /// (`docs/adr/0021` §8). The screen asks; the row decides.
    fn ask_about_policy(&self, intervention: atomcode_kernel::event::PolicyIntervention) {
        let options = policy_options(&intervention);
        if options.is_empty() {
            // Nothing to offer is not a question. Say what happened instead, so
            // the turn ending has a reason on screen.
            self.say("策略边界挡下了这一步,而这次没有给出可选的走法");
            return;
        }
        let question = atomcode_harness::seams::Question {
            prompt: "这一步被策略挡下了。接下来怎么走?".into(),
            options,
            asker: Some("策略".into()),
            about: None,
        };
        let answer = self.host.asks.push(question);
        let client = self.client.clone();
        tokio::spawn(async move {
            // A refusal — esc — leaves the intervention waiting rather than
            // picking something on the person's behalf. `/policy` is still
            // there when they decide.
            if let Some(chosen) = answer.await.ok().flatten() {
                client.invoke("policy", &chosen);
            }
        });
    }

    /// Put a line of the UI's own into the conversation.
    ///
    /// A block like any other, so it scrolls, folds and is dumped on exit with
    /// everything else — the alternative is a status line that has to be read
    /// before something overwrites it.
    fn say(&self, text: &str) {
        let mut stream = self.host.stream.write().expect("stream poisoned");
        let mut w = stream.writer("commands");
        w.emit(
            crate::block::Coord::default(),
            Arc::new(crate::content::CommandSaid {
                text: text.to_string(),
                refused: false,
            }),
        );
    }

    /// The agent's own events: they move the status line and answer the
    /// commands that asked for something, and nothing else. Content is not
    /// read from here — the transcript folds the log.
    ///
    /// Returns whether any of that changed what is drawn. Most of what arrives
    /// does not: a streamed delta is in the transcript already, and this event
    /// is the same news in another shape, so a frame for it would recompose the
    /// picture that is already on the screen.
    fn on_event(&self, event: AgentEvent) -> bool {
        use crate::moment::Activity;
        match event {
            // Content: a fact of the session on screen, folded once.
            AgentEvent::Fact(committed) => {
                if !self.client.keep(&committed) {
                    return false;
                }
                self.host
                    .absorb_logged(&atomcode_kernel::session::LoggedEvent {
                        seq: committed.seq,
                        at: committed.at,
                        event: committed.event.clone(),
                    });
                // What an undo took back is the whole log's to say, so it is
                // read off the facts rather than folded fact by fact.
                if matches!(
                    committed.event,
                    atomcode_kernel::session::SessionEvent::Rewound { .. }
                        | atomcode_kernel::session::SessionEvent::Interrupted { .. }
                ) {
                    self.mark_undone();
                }
                true
            }
            AgentEvent::Described { description } => {
                self.client.describe(&description);
                false
            }
            AgentEvent::Accepted { command, .. } => {
                self.client.answered(&command);
                false
            }
            AgentEvent::Rejected { command, error } => {
                self.client.answered(&command);
                self.say_refused(&format!("没有送达:{error:?}"));
                true
            }
            AgentEvent::Request { id, kind, payload } => {
                self.ask(id, &kind, payload);
                true
            }
            // A hard policy boundary stopped the turn and the person has to say
            // how to go on (`docs/adr/0021` §8,
            // `docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A7).
            // It used to arrive and vanish: the turn ended, the screen said
            // nothing, and the only way on was to know that `/policy` existed.
            AgentEvent::PolicyIntervention { intervention } => {
                self.ask_about_policy(intervention);
                true
            }
            AgentEvent::Invoked { output, .. } => {
                if !output.is_empty() {
                    self.say(&output);
                }
                true
            }
            // The session's own status follows its turn events below; a
            // member's is the member strip's.
            AgentEvent::StatusChanged { session, status } => {
                let on_screen = self.client.status(&session, status);
                let mut changed = false;
                if let Some(member) = self
                    .members
                    .lock()
                    .expect("members poisoned")
                    .get_mut(&session)
                {
                    if status == AgentStatus::Working && member.status != AgentStatus::Working {
                        member.turns += 1;
                    }
                    member.status = status;
                    changed = true;
                }
                // A member on screen has no turn events on this connection —
                // those are the root's — so its status is what moves the line.
                if on_screen && session != self.client.root() {
                    use crate::moment::Activity;
                    changed |= self.set_activity(match status {
                        AgentStatus::Idle => Activity::Idle,
                        AgentStatus::Working => Activity::Working,
                        AgentStatus::Stopping => Activity::Stopping,
                    });
                }
                changed
            }
            AgentEvent::AgentAdded { description } => {
                if description.parent.as_deref() != Some(self.client.root().as_str()) {
                    return false;
                }
                self.client.member(&description.session, true);
                let name = description
                    .member
                    .as_ref()
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| {
                        description
                            .session
                            .rsplit('/')
                            .next()
                            .unwrap_or(&description.session)
                            .to_string()
                    });
                self.members.lock().expect("members poisoned").insert(
                    description.session.clone(),
                    Member {
                        name,
                        status: AgentStatus::Idle,
                        turns: 0,
                        gone: false,
                    },
                );
                true
            }
            AgentEvent::AgentRemoved { session } => {
                self.client.member(&session, false);
                // Kept on the panel: a stopped member's log is still there to
                // look at (`docs/adr/0023` §5).
                self.members
                    .lock()
                    .expect("members poisoned")
                    .get_mut(&session)
                    .map(|member| member.gone = true)
                    .is_some()
            }
            // The turn events on this connection are the root's: a member on
            // screen is moved by its status instead.
            AgentEvent::TurnStarted { .. }
            | AgentEvent::TurnComplete { .. }
            | AgentEvent::Cancelled
            | AgentEvent::Steered { .. }
                if self.client.session() != self.client.root() =>
            {
                false
            }
            AgentEvent::TurnStarted { .. } => self.set_activity(Activity::Working),
            AgentEvent::TurnComplete { .. } | AgentEvent::Cancelled => {
                // A cancel or a failure can end the turn with words still in the
                // inbox — nothing folded them, and no `Steered` is coming. The
                // panel is a claim about the model's inbox, so it goes with the
                // turn rather than lying about work that will not happen.
                self.host.clear_steering();
                self.set_activity(Activity::Idle)
            }
            AgentEvent::Steered { .. } => {
                // The model has been handed what was waiting. This is the moment
                // the transcript starts drawing it too, so the panel leaves as it
                // arrives rather than showing the same sentence twice. The `true`
                // is the panel leaving — the transcript's half of the exchange
                // comes to the screen as the fact itself, which needs no frame
                // from here.
                self.host.clear_steering();
                true
            }
            AgentEvent::Compacted { committed, .. } => {
                if committed {
                    self.say("已压缩");
                } else {
                    self.say("暂时没有值得压缩的");
                }
                true
            }
            AgentEvent::Error { message, .. } => {
                self.set_activity(Activity::Idle);
                self.say_refused(&message);
                true
            }
            _ => false,
        }
    }

    /// Tell the status line what the agent is doing. `false` when it already
    /// said so: the frame would be the one already up.
    ///
    /// The host's, not this front end's: writing `activity` moves the live line,
    /// and the live line is a row of the scrollable content now — so whoever
    /// writes it has to hold the reading still across the change. That is
    /// [`Host::set_activity`]'s job, and keeping it there is what stops this
    /// route and the fact route from pinning separately.
    fn set_activity(&self, activity: crate::moment::Activity) -> bool {
        self.host.set_activity(activity)
    }

    fn say_refused(&self, text: &str) {
        let mut stream = self.host.stream.write().expect("stream poisoned");
        let mut w = stream.writer("commands");
        w.emit(
            crate::block::Coord::default(),
            Arc::new(crate::content::CommandSaid {
                text: text.to_string(),
                refused: true,
            }),
        );
    }

    /// Apply one action. Returns `true` to quit.
    fn act(&self, action: Action, client: &AgentClient) -> bool {
        let mut m = self.host.moment.write().expect("moment poisoned");
        // A highlight is a rectangle of screen cells. Anything that repaints
        // those cells with different text leaves it pointing at the wrong
        // words, so it is dropped by everything except the gestures that are
        // *about* it.
        if !matches!(
            action,
            Action::SelectFrom(..)
                | Action::SelectTo(..)
                | Action::CopySelection
                | Action::ClearSelection
        ) {
            // Escape does the innermost thing, and the selection is the
            // innermost of them.
            if m.selection.take().is_some() && matches!(action, Action::Escape) {
                return false;
            }
        }
        // Typing means this is yours now, not the entry you arrowed back to.
        // The stashed draft goes: there is only one thing being composed.
        if matches!(
            action,
            Action::Insert(_)
                | Action::Backspace
                | Action::DeleteWord
                | Action::Paste(_)
                | Action::AttachImage
        ) {
            m.history_at = None;
        }
        match action {
            Action::Quit => return true,
            Action::Submit => {
                let text = m.input.trim().to_string();
                // Take the pictures the text still shows before the text is
                // cleared: what was written and what was attached have to be
                // decided together, or an attachment can outlive the marker
                // that was the only reason it was going.
                let images = m.attachments.take_shown(&text);
                m.input.clear();
                m.caret = 0;
                m.history_at = None;
                m.draft.clear();
                m.scroll = crate::moment::ScrollPos::BOTTOM;
                drop(m);
                self.refresh_menu();
                if text.is_empty() {
                    return false;
                }
                // A slash goes to the command surface, everything else to the
                // model. The one place the two are told apart.
                if text.starts_with('/') {
                    self.run_command(&text);
                    return false;
                }
                // One command. Whether it starts a turn or folds into the one
                // running is the pump's call, not the screen's.
                //
                // What the screen does know is whether a turn is *already*
                // running, and only then is this steering. Said between turns it
                // opens a new turn and reaches the model immediately — there is
                // no gap to fill, and `Steered` will never come to close a panel
                // that was opened for it.
                let steering = self.host.moment.read().expect("moment poisoned").activity
                    != crate::moment::Activity::Idle;
                client.send(text.clone(), images);
                if steering {
                    self.host.add_steering(&text);
                }
                return false;
            }
            Action::Insert(c) => {
                let at = m.caret.min(m.input.len());
                m.input.insert(at, c);
                m.caret = at + c.len_utf8();
            }
            Action::Backspace => {
                if m.caret > 0 {
                    let mut at = m.caret - 1;
                    while at > 0 && !m.input.is_char_boundary(at) {
                        at -= 1;
                    }
                    m.input.remove(at);
                    m.caret = at;
                }
            }
            Action::DeleteWord => {
                let caret = m.caret;
                // Safe on the caret's invariant, not on luck: every writer of
                // `m.caret` above lands it on a character boundary — insert
                // adds `len_utf8`, the arrows and backspace walk to
                // `is_char_boundary`, a click goes through
                // `input::offset_at`, which adds the byte length of a
                // `take_width` prefix. Add a sixth writer and it must do the
                // same, or this is where it panics.
                #[allow(
                    clippy::string_slice,
                    reason = "the caret is kept on a character boundary by every writer of it"
                )]
                let head = m.input[..caret].trim_end();
                let cut = head.rfind(' ').map(|i| i + 1).unwrap_or(0);
                m.input.replace_range(cut..caret, "");
                m.caret = cut;
            }
            Action::Clear => {
                m.input.clear();
                // The markers went with the text, so the images they stood for
                // go too. Leaving them held would make the composer's state
                // disagree with the only place a person can see it.
                m.attachments.clear();
                m.caret = 0;
            }
            Action::CaretLeft => {
                let mut at = m.caret.saturating_sub(1);
                while at > 0 && !m.input.is_char_boundary(at) {
                    at -= 1;
                }
                m.caret = at;
            }
            Action::CaretRight => {
                let mut at = (m.caret + 1).min(m.input.len());
                while at < m.input.len() && !m.input.is_char_boundary(at) {
                    at += 1;
                }
                m.caret = at;
            }
            Action::CaretHome => m.caret = 0,
            Action::CaretEnd => m.caret = m.input.len(),

            // Inside the text they move the caret; at its edge they hand over
            // to the history. That is what an arrow key does in a shell, and
            // the composer is now tall enough for the first half to matter.
            Action::CaretUp | Action::CaretDown => {
                use crate::modules::input;
                let up = matches!(action, Action::CaretUp);
                let w = self.surface.size().0;
                let (row, col, rows) = input::caret_row(&m.input, m.caret, w);
                let inside = if up { row > 0 } else { row + 1 < rows };
                if inside {
                    // The column is kept, the way a text editor keeps it:
                    // moving down and back up lands where it started.
                    let to = if up { row - 1 } else { row + 1 };
                    m.caret = input::offset_at(&m.input, to, col, w);
                } else if up {
                    recall_back(&mut m);
                } else {
                    recall_forward(&mut m);
                }
                return false;
            }
            Action::Paste(text) => {
                let text = sanitize_paste(&text);
                let at = m.caret.min(m.input.len());
                m.input.insert_str(at, &text);
                m.caret = at + text.len();
            }
            Action::AttachImage => {
                // The destination is decided before the clipboard is even read.
                // A model that would drop the bytes has to say so now, while the
                // person is still holding the screenshot, rather than after they
                // have typed a question about a picture that never left.
                if let Err(reason) = images_reach_the_model(client) {
                    drop(m);
                    self.say_refused(&reason);
                    return false;
                }
                // Read first, mutate second: a clipboard with no image in it
                // must leave the composer exactly as it was, and "exactly as it
                // was" is easier to keep true when nothing was touched yet.
                let Some(image) = self.surface.clipboard_image() else {
                    drop(m);
                    self.say_refused("剪贴板里没有图片");
                    return false;
                };
                let label = m.attachments.add(image);
                // At the caret, not appended: the marker is part of the
                // sentence, and where it lands is where the person put it.
                let at = m.caret.min(m.input.len());
                // Safe on the caret's invariant, the same one `DeleteWord`
                // relies on: every writer of `m.caret` leaves it on a character
                // boundary.
                #[allow(
                    clippy::string_slice,
                    reason = "the caret is kept on a character boundary by every writer of it"
                )]
                let head = &m.input[..at];
                let gap = if !head.is_empty() && !head.ends_with(char::is_whitespace) {
                    " "
                } else {
                    ""
                };
                let inserted = format!("{gap}{label}");
                m.input.insert_str(at, &inserted);
                m.caret = at + inserted.len();
            }
            Action::Cancel => {
                // Through the host, so the live line's appearance is pinned the
                // same way a turn's start is: this is the third route that moves
                // that row, and a pin on two of three jumps on the third.
                drop(m);
                self.host.set_activity(crate::moment::Activity::Stopping);
                client.cancel();
                return false;
            }
            Action::Scroll(by) => {
                // Bounded by what is left to read, not by how much there is:
                // scrolling past the oldest line would empty the region rather
                // than show more of it.
                let max = self.host.scroll_limit(self.surface.size(), &m);
                let next = m.scroll.0 as i64 - by as i64;
                m.scroll = crate::moment::ScrollPos(next.clamp(0, max as i64) as usize);
            }
            Action::ScrollToBottom => m.scroll = crate::moment::ScrollPos::BOTTOM,
            // A click on a block, resolved against the frame that was actually
            // painted. Anywhere else — the prompt, the status line, a gap — is
            // not an error, it is simply not a fold.
            Action::ClickAt(x, y) => {
                // The badge sits on top of a row of the stream, and the thing
                // on top is the thing that was clicked.
                if self.host.jump_at(x, y) {
                    m.scroll = crate::moment::ScrollPos::BOTTOM;
                    return false;
                }
                // Inside the composer a click is a caret, not a fold. Checked
                // before the stream because the two never overlap and this is
                // the cheaper answer.
                if let Some(rect) = self.host.field_rect() {
                    if let Some(at) = crate::modules::input::offset_at_cell(&m, rect, x, y) {
                        m.caret = at;
                        return false;
                    }
                }
                drop(m);
                let Some((id, kind)) = self.host.block_at(x, y) else {
                    return false;
                };
                // Anchor the block that was clicked, not the bottom of the
                // conversation. A block that grows pushes its own header off
                // the top — click a tool call and the line you clicked is the
                // first thing to leave. Moving the view back by exactly what it
                // gained keeps that line where it was, and what appears, appears
                // *below* it.
                // `m` was dropped above, so the pin can take the moment it
                // needs without the caller's write lock in the way. Not a
                // fourth copy of the arithmetic: folding a block changes how
                // many rows there are to read, which is what every other route
                // pins against.
                //
                // Held **whether or not the reader is at the bottom**, unlike
                // the routes that fold a fact. The person pointed at this row;
                // unfolding grows the block upward and its header is the first
                // thing to leave, so the row they pointed at has to stay where
                // they pointed — and pointing at a row while at the bottom is
                // the common case, not the exception.
                self.host.held_while(|| self.host.toggle_block(id, kind));
                return false;
            }
            // Handing the mouse back is the answer to "I cannot select text
            // any more", so the answer has to say so where it will be read —
            // and say how to get selection without giving the pointer up at
            // all, which most people would rather do.
            Action::ToggleMouse => {
                drop(m);
                let on = !self.surface.mouse();
                self.surface.set_mouse(on);
                let text = if on {
                    "鼠标已收回:拖动选中并复制,点击思考或工具调用折叠展开那一个,滚轮滚动,esc 取消选中"
                        .to_string()
                } else {
                    "鼠标已交还终端:改用终端自己的框选(可跨 scrollback)。折叠用 ctrl-t,思考用 ctrl-r(默认不显示),滚动用 pgup/pgdn,ctrl-o 收回鼠标"
                        .to_string()
                };
                self.say(&text);
                return false;
            }

            Action::SelectFrom(x, y) => {
                m.selection = Some(crate::moment::Selection::at(x, y));
                return false;
            }
            Action::SelectTo(x, y) => {
                if let Some(sel) = m.selection.as_mut() {
                    sel.head = (x, y);
                }
                return false;
            }
            Action::ClearSelection => {
                m.selection = None;
                return false;
            }
            // Copy on release, the way a terminal does it. The highlight stays
            // up afterwards so a person can see what they took.
            Action::CopySelection => {
                let Some(sel) = m.selection else {
                    return false;
                };
                drop(m);
                let text = self.host.compose(self.surface.size()).selected_text(&sel);
                if !text.is_empty() {
                    self.surface.copy(&text);
                }
                return false;
            }
            // One layer at a time: the selection (above), then what is typed,
            // then the turn. Clearing a draft you were still writing is
            // annoying; losing it because you wanted to stop the model is
            // worse, which is why ctrl-c stays `Cancel` and only stops.
            Action::Escape => {
                if !m.input.is_empty() {
                    m.draft.clear();
                    m.history_at = None;
                    m.input.clear();
                    m.caret = 0;
                    return false;
                }
                drop(m);
                self.host.set_activity(crate::moment::Activity::Stopping);
                client.cancel();
                return false;
            }
            Action::Newline => {
                let at = m.caret.min(m.input.len());
                m.input.insert(at, '\n');
                m.caret = at + 1;
                m.history_at = None;
                return false;
            }
            Action::Redraw => {
                drop(m);
                self.surface.forget();
                return false;
            }
            Action::ToggleFold(kind) => {
                drop(m);
                self.host
                    .presentation
                    .write()
                    .expect("presentation poisoned")
                    .toggle(kind);
                return false;
            }
            Action::ToggleFolds(kinds) => {
                drop(m);
                self.host
                    .presentation
                    .write()
                    .expect("presentation poisoned")
                    .toggle_many(kinds);
                return false;
            }
        }
        drop(m);
        // Every edit to the line can change what the menu should show.
        self.refresh_menu();
        false
    }

    /// Route one key to the question on screen. Returns `true` to quit.
    ///
    /// A question is a panel riding the stream's tail, not a modal, so the keys come
    /// here directly — there is no overlay that holds the keyboard first. What has
    /// not changed is the rule: while a question is waiting, every key is the
    /// question's. Exactly one thing can hold focus, and this is what holds it.
    ///
    /// **Every answer is reachable by one key.** Up/down walk the list, which is what
    /// a highlighted row is for; enter takes the row that is lit; a digit or a first
    /// letter goes straight to an answer, which is faster once the list is known; esc
    /// declines. The moves and the picks both live in [`crate::ask::Pending`], so the
    /// keys the panel shows and the keys the fallback answers are one implementation
    /// rather than two that agree until one changes.
    fn answer_question(&self, press: crate::surface::KeyPress) -> bool {
        use crate::surface::{Key, Mods};
        let Some((_, question)) = self.host.asks.peek() else {
            return false;
        };
        let chosen = match (press.key, press.mods) {
            (Key::Up, _) | (Key::Char('k'), Mods::CTRL) => {
                let _ = self.host.move_ask_by(-1);
                None
            }
            (Key::Down, _) | (Key::Char('j'), Mods::CTRL) => {
                let _ = self.host.move_ask_by(1);
                None
            }
            // Esc and ctrl-c decline. Declining is an answer; it is never consent,
            // and it must always be one keystroke away.
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => Some(None),
            (Key::Char('d'), Mods::CTRL) => return true,
            (Key::Char(c), _) if c.is_ascii_digit() => {
                crate::ask::nth(&question, c.to_digit(10).unwrap_or(0) as usize).map(Some)
            }
            // A letter picks the answer that starts with it. It does *not* fall back
            // to typing: a question's answers are what there is to choose between, and
            // a stray character is not one of them.
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
                crate::ask::by_prefix(&question, c).map(Some)
            }
            // Enter takes the row that is pointed at. With several answers that is
            // the highlighted one — which is what the highlight is *for*, and why it
            // starts on the first: a stray return takes what the screen shows it
            // would take, never a hidden default.
            //
            // Unmounted, there is no highlight to trust: the fallback at the foot of
            // the stream marks nothing, so enter keeps the rule it had there and
            // takes an answer only when there is exactly one to take.
            (Key::Enter, _) if self.panel_mounted() => Some(self.pointed_at()),
            (Key::Enter, _) if question.options.len() == 1 => Some(self.pointed_at()),
            _ => None,
        };
        let Some(answer) = chosen else {
            return false; // a move, or an unrecognised key: nothing to deliver
        };
        if let Some(p) = self.host.asks.take() {
            p.answer(answer);
        }
        false
    }

    /// Whether a question is being drawn as a panel rather than at the foot of the
    /// stream.
    ///
    /// Read off `Moment::asking`, which the host sets only for a mounted panel —
    /// so this is the same question the renderer answered, not a second opinion
    /// about it.
    fn panel_mounted(&self) -> bool {
        self.host
            .moment
            .read()
            .expect("moment poisoned")
            .asking
            .is_some()
    }

    /// The answer the panel has lit, as it would be delivered.
    ///
    /// One reader for both the return key and a click on a row, so a confirm and a
    /// pick cannot disagree about what is pointed at. Nothing lit — the question
    /// arrived this frame and has not been synced — is the first answer, because that
    /// is the one the panel would have lit.
    fn pointed_at(&self) -> Option<String> {
        let m = self.host.moment.read().expect("moment poisoned");
        match m.asking.as_ref() {
            Some(ask) => ask.picked(),
            None => self
                .host
                .asks
                .peek()
                .and_then(|(_, q)| q.options.first().map(|a| a.value.clone())),
        }
    }

    /// Deliver whatever the panel is pointed at. `true` to quit, for the key path.
    fn confirm_question(&self) -> bool {
        let answer = self.pointed_at();
        if let Some(p) = self.host.asks.take() {
            p.answer(answer);
        }
        false
    }

    fn refresh_menu(&self) {
        let typed = self
            .host
            .moment
            .read()
            .expect("moment poisoned")
            .input
            .clone();
        let menu = match typed.strip_prefix('/') {
            Some(rest) if !rest.contains(char::is_whitespace) => self
                .host
                .commands
                .matching(rest)
                .into_iter()
                .map(|c| {
                    let name = match &c.takes {
                        Some(t) => format!("{} {t}", c.name),
                        None => c.name.to_string(),
                    };
                    (name, c.about.to_string())
                })
                .collect(),
            _ => Vec::new(),
        };
        self.host.set_menu(menu);
    }

    /// Handle a pointer press against the composer's context menu.
    ///
    /// `None` means "this press was not the menu's business" — either nothing
    /// was open, or the press was the right button that opens it. `Some(quit)`
    /// means it was, and the event should not be looked at again: a press on a
    /// menu row is a choice, and a press elsewhere while a menu is up closes it
    /// and is *also* allowed to keep meaning whatever it meant — which is why
    /// `Outside` returns `Some` only after the menu is already closed but lets
    /// the caller decide, by returning `None` for it below.
    fn pointer_in_menu(
        &self,
        click: crate::surface::Click,
        x: u16,
        y: u16,
        client: &AgentClient,
    ) -> Option<bool> {
        use crate::host::ContextClick;
        use crate::surface::Click;
        // A move is not a press. It is the menu following the pointer, not the
        // pointer asking the menu for something — and letting it through here
        // would make a menu pick, and close on, the row the pointer merely
        // crossed on its way somewhere else.
        if matches!(click, Click::Hover) {
            return None;
        }
        // The gesture that opens it is not handled by it.
        if matches!(click, Click::RightPress) {
            self.open_composer_menu(x, y);
            return Some(false);
        }
        let size = self.surface.size();
        match self.host.context_menu_click(x, y, size) {
            ContextClick::NotOpen => None,
            ContextClick::Picked(crate::menu::Step::Picked(value)) => {
                Some(self.run_menu_item(&value, client))
            }
            // Esc would have gone through the keyboard path; a click outside is
            // the same dismissal, and the press still means what it meant.
            ContextClick::Picked(_) | ContextClick::Outside => None,
        }
    }

    /// The menu a right-click opens, and what is in it.
    ///
    /// Where the press landed decides what can be done there. `清空` and `发送`
    /// act on what is being typed, and a press over the conversation is about the
    /// text it landed on — offering them there would be a menu whose items
    /// belong to a box that is not under the pointer. So outside the composer
    /// the menu keeps the one thing that is still true of the press: copying
    /// what is selected. Inside it, the four the composer has.
    ///
    /// The press is answered from the last painted frame (`Host::field_rect`)
    /// rather than from a fresh compose: which rect the field had is what was on
    /// screen when the button went down, and re-deriving it here could disagree
    /// with the picture the person was looking at.
    fn open_composer_menu(&self, x: u16, y: u16) {
        // What "copy" will act on, named: a selection is what this menu is
        // about when there is one, and a label that says "全文" over a selection
        // is the menu lying about what the next key press will do.
        let selected = {
            let m = self.host.moment.read().expect("moment poisoned");
            m.selection.is_some_and(|s| !s.is_empty())
        };
        let copy = if selected {
            crate::menu::Item::new("copy", "复制选中").about("把选中的文字写到剪贴板")
        } else {
            crate::menu::Item::new("copy", "复制全文").about("把输入框写到剪贴板")
        };
        let in_composer = self
            .host
            .field_rect()
            .is_some_and(|field| field.contains(x, y));
        let items = if in_composer {
            vec![
                copy,
                crate::menu::Item::new("paste", "粘贴").about("从剪贴板插入"),
                crate::menu::Item::new("clear", "清空").about("丢掉草稿和附件"),
                crate::menu::Item::new("send", "发送").about("把这一条交给模型"),
            ]
        } else if selected {
            // Over the conversation with something selected: the press is about
            // those words, so copying them is the whole menu. No second entry
            // and nothing that acts on the composer.
            vec![copy]
        } else {
            // Nothing was selected, so there is nothing a press here can ask
            // for. An empty menu opens nothing rather than a menu of verbs that
            // would act somewhere the pointer is not.
            Vec::new()
        };
        self.host.open_context_menu((x, y), items);
    }

    /// Do what a menu item says. Returns `true` to quit.
    ///
    /// A thin adapter over the same [`Action`]s the keys produce — the menu is a
    /// third way to ask for something the keymap already knows how to do, which
    /// is the point of `Action` being the vocabulary all three share. Nothing
    /// here is a second implementation of paste or send.
    fn run_menu_item(&self, value: &str, client: &AgentClient) -> bool {
        match value {
            "copy" => {
                // A selection first: it is what is on screen and what the menu
                // is about when it exists. The field is the fallback, not the
                // first answer — `复制全文` sent a selection to the clipboard as
                // the composer's contents, which on a right-click over selected
                // text copied the wrong thing entirely.
                let selected = {
                    let m = self.host.moment.read().expect("moment poisoned");
                    m.selection.filter(|s| !s.is_empty())
                };
                if let Some(sel) = selected {
                    let text = self.host.compose(self.surface.size()).selected_text(&sel);
                    if !text.is_empty() {
                        self.surface.copy(&text);
                        // The reserved row above the field, not the stream: this
                        // is true of *now*, and a block for it would push the
                        // conversation up a row for a sentence nobody reads
                        // twice.
                        self.host.say("已复制选中的内容", false);
                        return false;
                    }
                }
                let text = self
                    .host
                    .moment
                    .read()
                    .expect("moment poisoned")
                    .input
                    .clone();
                if text.is_empty() {
                    // Not an error: a person may open the menu on an empty
                    // composer to see what is there. Say why nothing happened,
                    // and name the thing that was empty — on the tip row, since
                    // nothing happened and nothing belongs in the conversation.
                    self.host.say(
                        if selected.is_some() {
                            "选中的内容没有可复制的文字"
                        } else {
                            "没有可复制的内容"
                        },
                        true,
                    );
                    return false;
                }
                self.surface.copy(&text);
                self.host.say("已复制到剪贴板", false);
                false
            }
            "paste" => {
                let Some(text) = self.surface.clipboard_text() else {
                    // The menu's own refusal, so it belongs on the tip row with
                    // the rest of them rather than in the conversation.
                    self.host.say("剪贴板里没有文本", true);
                    return false;
                };
                self.act(Action::Paste(text), client)
            }
            "clear" => self.act(Action::Clear, client),
            "send" => self.act(Action::Submit, client),
            _ => false,
        }
    }

    /// Run a slash command and put what it said on the screen.
    ///
    /// On its own task: a command may reconfigure the tree or call a model, and
    /// the loop must keep painting and keep accepting keys while it does.
    fn run_command(&self, line: &str) {
        let (Some(ctx), Some(keys)) = (
            self.ctx.lock().expect("ctx poisoned").clone(),
            self.wake.lock().expect("wake poisoned").clone(),
        ) else {
            return;
        };
        let commands = self.host.commands.clone();
        let host = self.host.clone();
        let line = line.to_string();
        tokio::spawn(async move {
            let outcome = commands.dispatch(&line, &ctx).await;
            let said = match outcome {
                crate::command::Outcome::Said(text) => Some((text, false)),
                crate::command::Outcome::Refused(why) => Some((why, true)),
                crate::command::Outcome::Quiet => None,
                crate::command::Outcome::Open(modal) => {
                    let keys2 = keys.clone();
                    host.overlays.open(
                        modal,
                        Box::new(move |chosen| {
                            let _ = keys2.send(Wake::Chose(chosen));
                        }),
                    );
                    let _ = keys.send(Wake::Fact);
                    None
                }
                crate::command::Outcome::Do(action) => {
                    // A command and a key share one implementation, so this is
                    // the same path a keystroke takes.
                    let _ = keys.send(Wake::Act(action));
                    None
                }
            };
            if let Some((text, refused)) = said {
                let mut stream = host.stream.write().expect("stream poisoned");
                let mut w = stream.writer("commands");
                w.emit(
                    crate::block::Coord::default(),
                    Arc::new(crate::content::CommandSaid { text, refused }),
                );
                drop(stream);
                let _ = keys.send(Wake::Fact);
            }
        });
    }

    /// A modal closed. Whatever it was picking, the picking is done here so a
    /// modal never needs the tree, the agent or the loop.
    fn chose(&self, chosen: Option<String>) {
        let Some(value) = chosen else { return };
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return;
        };
        let keys = self.wake.lock().expect("wake poisoned").clone();
        let host = self.host.clone();
        // A pick is expressed as a command, so a modal and a typed command
        // reach the same implementation — the same rule keys already follow.
        tokio::spawn(async move {
            let outcome = host.commands.dispatch(&value, &ctx).await;
            let said = match outcome {
                crate::command::Outcome::Said(t) => Some((t, false)),
                crate::command::Outcome::Refused(w) => Some((w, true)),
                _ => None,
            };
            if let Some((text, refused)) = said {
                let mut stream = host.stream.write().expect("stream poisoned");
                let mut w = stream.writer("commands");
                w.emit(
                    crate::block::Coord::default(),
                    Arc::new(crate::content::CommandSaid { text, refused }),
                );
            }
            if let Some(k) = keys {
                let _ = k.send(Wake::Fact);
            }
        });
    }

    /// Print the conversation to the normal buffer on the way out.
    fn dump(&self) {
        use std::io::Write;
        let (w, _) = self.surface.size();
        let text = {
            let stream = self.host.stream.read().expect("stream poisoned");
            transcript_text(&stream, w)
        };
        if !text.is_empty() {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
        }
    }
}

/// The conversation as it is handed back to the normal buffer on exit.
///
/// The same rule as a drawn row, for the same reason: this text came from a
/// file, a tool or the model, and it is on its way to a terminal. See
/// [`crate::text`]. The screen has already been restored when this is printed,
/// so these bytes land in the person's own buffer, next to the shell prompt —
/// and there is no repaint diff down here to bound the damage. A tool result
/// carrying `ESC[2J` clears the scrollback this exists to hand back, a `\r`
/// progress bar rewrites the line it lands on, and a tab-indented file returns
/// as columns the terminal resolved with a stop we never counted.
fn transcript_text(stream: &Stream, width: u16) -> String {
    let mut out = String::new();
    for slot in stream.slots() {
        for line in slot
            .block()
            .content
            .lines(&crate::block::RenderCtx::bare(width.max(20)))
        {
            out.push_str(&crate::text::for_screen(&line.plain()));
            out.push('\n');
        }
    }
    out
}

/// Back one entry in the history, stashing the draft on the way in.
fn recall_back(m: &mut crate::moment::Moment) {
    if m.history.is_empty() {
        return;
    }
    let at = match m.history_at {
        None => {
            m.draft = m.input.clone();
            m.history.len() - 1
        }
        Some(0) => return, // already at the oldest; going further is nowhere
        Some(i) => i - 1,
    };
    m.history_at = Some(at);
    m.input = m.history[at].clone();
    m.caret = m.input.len();
}

/// Forward one entry, and out the far side to the draft that was set aside.
fn recall_forward(m: &mut crate::moment::Moment) {
    let Some(at) = m.history_at else {
        return; // already composing; there is nothing newer than now
    };
    if at + 1 < m.history.len() {
        m.history_at = Some(at + 1);
        m.input = m.history[at + 1].clone();
    } else {
        m.history_at = None;
        m.input = std::mem::take(&mut m.draft);
    }
    m.caret = m.input.len();
}

/// Whether a picture attached to this conversation would actually reach the
/// model. `Err` is the reason it would not, phrased for the person.
///
/// The agent's own description is the only thing that can answer: an adapter
/// that cannot carry image content degrades it to a plain-text caption, which
/// is the right compromise for a conversation being *resumed* on a text-only
/// model and a silent loss for a screenshot someone pasted a moment ago.
/// Nothing in the screen could tell those apart, which is why the question is
/// asked of what the agent said about itself instead of guessed from a name.
///
/// Not knowing yet is refused rather than waved through: a picture with no
/// known destination is not sent by omission.
///
/// This runs when the picture is taken, not when the message is sent, so it
/// rests on one assumption: that the model cannot change between the two. A
/// change arrives as a new description, and a person switching models in the
/// middle of writing about a picture is the case that would break it.
fn images_reach_the_model(client: &AgentClient) -> Result<(), String> {
    match client.described() {
        Some(described) if described.supports_vision => Ok(()),
        Some(described) => Err(format!(
            "当前模型 `{}` 看不了图片:贴进去也只会在发出去时被丢掉,所以没贴。\n\
             换成能看图的模型再贴。",
            described.model.unwrap_or_default()
        )),
        None => Err("还不知道这个 agent 用的是什么模型,图片没有去处,所以没贴。".to_string()),
    }
}

/// Pasted text is arbitrary bytes from somewhere else, and this crate has one
/// place that decides what those bytes are allowed to become — see
/// [`crate::text`]. Kept as a name here because the *policy* is the paste
/// policy: a newline is content, and the composer breaks rows on it.
fn sanitize_paste(text: &str) -> String {
    crate::text::for_buffer(text)
}

async fn read_input(wake: mpsc::UnboundedSender<Wake>) {
    use futures::StreamExt;
    let mut events = crossterm::event::EventStream::new();
    while let Some(Ok(event)) = events.next().await {
        if let Some(input) = crate::surface::from_crossterm(event) {
            if wake.send(Wake::Input(input)).is_err() {
                break;
            }
        }
    }
}

// ---- the rows -----------------------------------------------------------

/// Assemble an EMPTY screen over a surface: the registries, the layout, the
/// keymap, the event loop — and no panels.
///
/// The panels come from rows (`crate::rows`). That is the difference between a
/// UI that *has* a plugin registry and a UI that *is* assembled from plugins:
/// this function no longer knows that a status bar exists.
pub fn assemble(surface: Arc<dyn Surface>) -> (Arc<Host>, Tui) {
    let mods = Arc::new(Modules::new());
    let host = Arc::new(Host::new(mods, default_layout()));
    let mut keys = Keys::new();
    keys.add(&crate::keymap::Default_).expect("default keys");
    (
        host.clone(),
        Tui {
            client: Arc::new(AgentClient::default()),
            host,
            keys,
            surface,
            ctx: Mutex::new(None),
            wake: Mutex::new(None),
            pressed_at: Mutex::new(None),
            members: Mutex::new(BTreeMap::new()),
        },
    )
}

pub struct TuiUiPlugin;

#[async_trait]
impl Plugin for TuiUiPlugin {
    fn name(&self) -> &'static str {
        "ui-tui2"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["surface"]
    }
    fn provides(&self) -> &'static [&'static str] {
        // It owns the screen. The registries it provides are filled by other
        // rows — this row supplies the slots, not the contents. The agent is not
        // among them: it is in the host's App (`docs/adr/0022` §3).
        &[
            "ui",
            "tui-agent-client",
            "tui-modules",
            "tui-rasters",
            "tui-commands",
            "tui-layout",
        ]
    }
    fn description(&self) -> &'static str {
        "a full-screen terminal UI assembled from module rows"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        // Parsed only to reject a stale `config = { mascot = true }` loudly:
        // silently ignoring it would leave someone staring at a screen with no
        // cat and no explanation.
        let _row: Row = if config.is_null() {
            Row::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        // A hard dependency, declared in `inject`, so the surface row is always
        // mounted first. The terminal is a row like any other — not a fallback
        // baked in here, which is what made a headless tree quietly grab the
        // tty and fail on a machine with no terminal at all.
        let surface = ctx.require::<SurfaceSvc>().map_err(|e| e.to_string())?;
        let (host, tui) = assemble(surface);
        let _ = ctx
            .provide::<ModulesSvc>(host.modules.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<RastersSvc>(host.rasters.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<CommandsSvc>(host.commands.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<LayoutSvc>(host.layout.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<AgentClientSvc>(tui.client.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<UiSvc>(Arc::new(tui))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The real terminal, as a row.
pub struct TerminalSurfacePlugin;

/// `theme = "auto" | "dark" | "light"`.
///
/// `auto` asks the terminal for its background colour and follows the answer.
/// The other two are the escape hatch for a terminal that does not answer, or
/// answers wrongly through tmux or ssh.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SurfaceRow {
    #[serde(default)]
    theme: Option<String>,
    /// Report the pointer, so a tool call folds when clicked. Costs the
    /// terminal's own click-drag selection, which every terminal gives back
    /// under a modifier (Option on macOS, Shift elsewhere).
    #[serde(default = "yes")]
    mouse: bool,
}

fn yes() -> bool {
    true
}

/// The configured palette (`None` means "ask the terminal"), and whether to
/// report the pointer.
fn surface_row(config: &Value) -> Result<(Option<crate::theme::Theme>, bool), String> {
    let row: SurfaceRow = if config.is_null() {
        SurfaceRow {
            theme: None,
            mouse: true,
        }
    } else {
        serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
    };
    let mouse = row.mouse && !std::env::var("ATOMCODE_NO_MOUSE").is_ok_and(|v| v != "0");
    // The env var wins: it is how a person overrides one session without
    // editing the tree they share with everyone else.
    let named = std::env::var("ATOMCODE_THEME")
        .ok()
        .filter(|v| !v.is_empty())
        .or(row.theme);
    let theme = match named.as_deref() {
        None | Some("auto") => None,
        Some("dark") => Some(crate::theme::Theme::Dark),
        Some("light") => Some(crate::theme::Theme::Light),
        Some(other) => return Err(format!("theme `{other}` is not auto, dark or light")),
    };
    Ok((theme, mouse))
}

#[async_trait]
impl Plugin for TerminalSurfacePlugin {
    fn name(&self) -> &'static str {
        "surface-terminal"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["surface"]
    }
    fn description(&self) -> &'static str {
        "the terminal, full screen, restored on the way out"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let (theme, mouse) = surface_row(config)?;
        let term =
            Terminal::enter(theme, mouse).map_err(|e| format!("cannot take the terminal: {e}"))?;
        let _ = ctx
            .provide::<SurfaceSvc>(Arc::new(term))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Mount a headless surface instead of the terminal. The row that makes the
/// whole UI testable without a tty.
pub struct HeadlessSurfacePlugin;

#[derive(Debug, Deserialize)]
struct SizeRow {
    #[serde(default = "default_w")]
    width: u16,
    #[serde(default = "default_h")]
    height: u16,
}
fn default_w() -> u16 {
    80
}
fn default_h() -> u16 {
    24
}

#[async_trait]
impl Plugin for HeadlessSurfacePlugin {
    fn name(&self) -> &'static str {
        "surface-headless"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["surface"]
    }
    fn description(&self) -> &'static str {
        "paint into memory and keep every frame; no tty"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: SizeRow = if config.is_null() {
            SizeRow {
                width: 80,
                height: 24,
            }
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let _ = ctx
            .provide::<SurfaceSvc>(Headless::new(row.width, row.height))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod dump_tests {
    use super::transcript_text;
    use crate::block::{hash_of, Content, ContentHash, Coord, Stream};
    use crate::frame::Line;
    use std::sync::Arc;

    /// A tool result exactly as it came back: the bytes that were in the file,
    /// not the bytes that are safe to print.
    #[derive(Debug)]
    struct ToolOutput(&'static str);

    impl Content for ToolOutput {
        fn kind(&self) -> &'static str {
            "tool-output"
        }
        fn content_hash(&self) -> ContentHash {
            hash_of(&[self.0])
        }
        fn lines(&self, _ctx: &crate::block::RenderCtx) -> Vec<Line> {
            self.0.lines().map(Line::raw).collect()
        }
    }

    fn conversation(body: &'static str) -> Stream {
        let mut s = Stream::new();
        let mut w = s.writer("test");
        let id = w.open(Coord::new(1, 1), Arc::new(ToolOutput(body)));
        w.settle(id);
        s
    }

    #[test]
    fn the_text_handed_back_on_exit_can_never_move_the_cursor() {
        // On the way out the alternate screen is given back and this text is
        // printed into the person's own buffer, shell prompt and all. There is
        // no repaint diff down here to bound the damage, so a tool result
        // carrying an escape does not corrupt one row — it reaches whatever the
        // escape says: `ESC[2J` clears the scrollback this exists to hand back.
        let body = "1 a\rb\tc\x1b[31md\n2 \x1b[2Jwipe\n";
        assert!(
            body.contains('\r') && body.contains('\t') && body.contains('\x1b'),
            "the fixture must actually be hazardous, or this test proves nothing"
        );

        let text = transcript_text(&conversation(body), 80);
        assert!(
            !text.contains('\r'),
            "a CR rewrites the line it lands on: {text:?}"
        );
        assert!(
            !text.contains('\t'),
            "a tab is the terminal's stop, not ours: {text:?}"
        );
        assert!(
            !text.contains('\x1b'),
            "an escape clears the buffer we just handed back: {text:?}"
        );
        assert!(text.contains("ab"), "the text itself survives: {text:?}");
        assert!(text.contains("wipe"), "the text itself survives: {text:?}");
        assert!(
            text.ends_with('\n'),
            "and it still ends in a break: {text:?}"
        );
    }
}

#[cfg(test)]
mod history_tests {
    use super::{recall_back, recall_forward};
    use crate::moment::Moment;

    fn said(entries: &[&str], typing: &str) -> Moment {
        let mut m = Moment::default();
        m.history = entries.iter().map(|s| s.to_string()).collect();
        m.input = typing.to_string();
        m.caret = m.input.len();
        m
    }

    #[test]
    fn arrowing_back_walks_what_was_said_newest_first() {
        let mut m = said(&["first", "second", "third"], "");
        recall_back(&mut m);
        assert_eq!(m.input, "third");
        recall_back(&mut m);
        assert_eq!(m.input, "second");
        recall_back(&mut m);
        assert_eq!(m.input, "first");
        recall_back(&mut m);
        assert_eq!(m.input, "first", "the oldest is the end of the road");
        assert_eq!(m.caret, m.input.len(), "the caret follows to the end");
    }

    #[test]
    fn the_draft_is_set_aside_and_given_back() {
        // The thing a history that loses your half-written message gets wrong.
        let mut m = said(&["old"], "half a thought");
        recall_back(&mut m);
        assert_eq!(m.input, "old");
        recall_forward(&mut m);
        assert_eq!(m.input, "half a thought", "the draft came back");
        assert_eq!(m.history_at, None, "and we are composing again");
    }

    #[test]
    fn forward_from_a_draft_does_nothing_because_there_is_nothing_newer() {
        let mut m = said(&["old"], "mine");
        recall_forward(&mut m);
        assert_eq!(m.input, "mine");
        assert_eq!(m.history_at, None);
    }

    #[test]
    fn an_empty_history_is_not_a_special_case_anyone_has_to_handle() {
        let mut m = said(&[], "mine");
        recall_back(&mut m);
        recall_forward(&mut m);
        assert_eq!(m.input, "mine");
    }
}

#[cfg(test)]
mod policy_tests {
    use super::policy_options;
    use atomcode_kernel::event::{PolicyIntervention, PolicyRecoveryAction as A};

    /// A policy intervention becomes a question with the intervention's own
    /// ways out — and only those
    /// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A7).
    ///
    /// The words are the ones the `policy` command takes, so what a person
    /// picks is what runs. Offering a way out the kernel did not name would be
    /// offering something that gets refused after they choose it.
    #[test]
    fn a_policy_intervention_offers_its_own_ways_out_and_no_others() {
        let intervention = PolicyIntervention::credential_shell_blocked();
        let offered = policy_options(&intervention);
        assert_eq!(
            offered.len(),
            intervention.actions.len(),
            "one row per way out this intervention actually has: {:?}",
            intervention.actions
        );
        // Every row is named in words a person reads, and valued in the word
        // the command takes.
        for answer in &offered {
            assert!(
                ["done", "skip", "how", "end"].contains(&answer.value.as_str()),
                "unexpected value: {answer:?}"
            );
            assert!(!answer.label.is_empty(), "a row with no words: {answer:?}");
        }

        // An intervention with nothing on offer is not a question at all.
        let empty = PolicyIntervention {
            actions: Vec::new(),
            ..PolicyIntervention::credential_shell_blocked()
        };
        assert!(policy_options(&empty).is_empty());

        // And the order is the intervention's, not this screen's.
        let reordered = PolicyIntervention {
            actions: vec![A::EndTask, A::SkipStep],
            ..PolicyIntervention::credential_shell_blocked()
        };
        assert_eq!(
            policy_options(&reordered)
                .iter()
                .map(|a| a.value.clone())
                .collect::<Vec<_>>(),
            vec!["end".to_string(), "skip".to_string()]
        );
    }
}

#[cfg(all(test, unix))]
mod askpass_tests {
    use super::*;
    use crate::module::Mounted;
    use crate::modules::{input, status, transcript};
    use crate::surface::{Key, KeyPress, Mods};
    use atomcode_capabilities::askpass::server::AskpassPrompt;

    fn screen() -> Arc<crate::host::Host> {
        let mods = Arc::new(crate::module::Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        mods.add_view(Arc::new(Mounted::<status::Status>::new()))
            .unwrap();
        mods.add_view(Arc::new(Mounted::<input::Input>::new()))
            .unwrap();
        Arc::new(crate::host::Host::new(mods, crate::host::default_layout()))
    }

    fn press(key: Key) -> KeyPress {
        KeyPress {
            key,
            mods: Mods::NONE,
        }
    }

    fn typed(host: &crate::host::Host, text: &str) {
        for c in text.chars() {
            assert!(
                !host.overlays.key(press(Key::Char(c))),
                "typing does not close the prompt"
            );
        }
    }

    /// A password `sudo` asks for reaches the screen as a modal, and what the
    /// person types reaches the one waiting for it — which is what makes a
    /// `sudo` inside a tool call finish instead of hanging on a tty this screen
    /// owns (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` P0-1).
    #[tokio::test]
    async fn a_password_sudo_asks_for_is_answered_from_the_screen() {
        let host = screen();
        let (wake, mut woken) = mpsc::unbounded_channel();
        let (asking, prompts) = tokio::sync::mpsc::channel(4);
        let pump = tokio::spawn(answer_prompts(host.clone(), wake, prompts));

        let (reply, answered) = tokio::sync::oneshot::channel();
        asking
            .send(AskpassPrompt {
                prompt: "[sudo] password for lichao:".into(),
                key: "sudo".into(),
                reply,
            })
            .await
            .unwrap();
        // The loop is woken: the prompt arrives mid-turn, when nothing else is
        // waking it, and a modal nobody repaints is a modal nobody sees.
        // With a deadline: a screen that is never told to repaint would
        // otherwise make this judgement hang, and a hang reports as slow rather
        // than as wrong.
        let woke = tokio::time::timeout(std::time::Duration::from_secs(5), woken.recv()).await;
        assert!(
            matches!(woke, Ok(Some(Wake::Fact))),
            "the screen is told to repaint (timed out or wrong wake)"
        );
        let open = host.overlays.current().expect("a modal is up");
        assert_eq!(open.id(), "secret");
        assert!(
            open.title().contains("password for lichao"),
            "asked in the words the program used: {}",
            open.title()
        );

        typed(&host, "hunter2");
        assert!(
            host.overlays.key(press(Key::Enter)),
            "enter closes the prompt"
        );
        assert_eq!(answered.await.unwrap().as_deref(), Some("hunter2"));
        drop(asking);
        pump.await.unwrap();
    }

    /// Esc reaches the asking program as a refusal, never as an empty password:
    /// `sudo` given an empty one *tries* it and burns an attempt.
    #[tokio::test]
    async fn esc_reaches_sudo_as_a_refusal_not_as_an_empty_password() {
        let host = screen();
        let (wake, _woken) = mpsc::unbounded_channel();
        let (asking, prompts) = tokio::sync::mpsc::channel(4);
        let pump = tokio::spawn(answer_prompts(host.clone(), wake, prompts));

        let (reply, answered) = tokio::sync::oneshot::channel();
        asking
            .send(AskpassPrompt {
                prompt: "password:".into(),
                key: "sudo".into(),
                reply,
            })
            .await
            .unwrap();
        while host.overlays.current().is_none() {
            tokio::task::yield_now().await;
        }
        typed(&host, "half a password");
        assert!(host.overlays.key(press(Key::Esc)), "esc closes the prompt");
        assert_eq!(answered.await.unwrap(), None, "a refusal, not a blank");
        drop(asking);
        pump.await.unwrap();
    }

    /// The other half: a child process can find the server. Without the env
    /// vars and a script to exec, `sudo` never asks at all — it goes to the tty
    /// and hangs, which is the state this replaced.
    #[tokio::test]
    async fn a_child_process_can_find_the_prompt_we_would_answer() {
        let host = screen();
        let (wake, _woken) = mpsc::unbounded_channel();
        let guard = serve_askpass(host, wake).expect("the server starts");
        let env = atomcode_capabilities::askpass::current_env()
            .expect("the environment a child is given");
        assert!(env.sock_path.exists(), "the socket is there to connect to");
        assert!(
            env.askpass_script.is_file(),
            "and a script for sudo to exec: {:?}",
            env.askpass_script
        );
        assert!(!env.token.is_empty(), "with a token to authenticate");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&env.askpass_script)
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "executable: {mode:o}");
        drop(guard);
    }
}
