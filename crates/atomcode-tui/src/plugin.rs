//! The rows: how this UI mounts into a config tree of its own.
//!
//! The screen is an App apart from the agent it drives (`docs/adr/0022` §3).
//! It holds the surface, the panels, the commands and the layout; the agent's
//! App belongs to whoever hosts it, and reaches the screen as a
//! [`HostConnection`] — the handle protocol and host control. When the host
//! replaces the session, the screen keeps its connection and starts drawing
//! the new session (`docs/adr/0022` §6).

use crate::i18n::product::{t as pt, Msg as PMsg};
use crate::i18n::{t, Msg};
use std::collections::{BTreeMap, HashSet};

/// When a press landed, where, and how many in a row — the three facts a
/// double- or triple-click is made of.
///
/// A name rather than a tuple in the field: three anonymous parts, two of them
/// numbers, is a type whose meaning lives in a comment somewhere else.
type ClickStreak = (std::time::Instant, (u16, u16), u8);
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::seams::{UiSvc, UserInterface};
use atomcode_host_api::{HostCommand, HostConnection, HostControl, HostEvent, HostReply};
use atomcode_kernel::agent::{AgentDescription, AgentStatus};
use atomcode_kernel::event::{AgentCommand, AgentEvent, CommandId, RequestId};
use atomcode_kernel::session::{Committed, LoggedEvent, SeqNo};
use atomcode_plexus::{plexus_service, Context, Plugin};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

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
plexus_service!(KeysSvc => crate::keymap::Keys, "tui-keys", Core, "Key bindings contributed by rows");
plexus_service!(BrandSvc => crate::content::Brand, "tui-brand", Seam, "What this build calls itself: its name, its licence, its mascot");
// The welcome block's words, as a seam: a product that already has a
// localisation provides one and the block follows `/language` with the rest of
// the product. Left unfilled, the row uses the sentences this build ships
// (`modules::welcome::ShippedWords`), so a screen with no launcher still opens.
plexus_service!(WelcomeWordsSvc => dyn crate::content::WelcomeWords, "tui-welcome-words", Seam, "The welcome block's heading and tip descriptions, in the language in force");
plexus_service!(AgentClientSvc => AgentClient, "tui-agent-client", Core, "The screen's end of its connection to the agent");
// Declared here, by the one that consumes it (`docs/adr/0021` §6): whoever
// launches the screen fills it with what its host handed over.
plexus_service!(ConnectionSvc => Connection, "agent-connection", Seam, "What the host handed this screen: its agent and host control");
// Provided by the loop once it exists, so whatever is working outside it can
// say that what is on screen changed.
plexus_service!(RepaintSvc => dyn Repaint, "tui-repaint", Seam, "Ask for a frame from outside the loop");

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

// The settings, as the launcher reads them. A seam rather than a core service,
// and declared by the one that consumes it (`docs/adr/0021` §6), like
// `ConnectionSvc`: which file the configuration lives in, and what writing it
// back means, belong to the product. A screen that read the file itself would be
// reading the product's own state, which is what makes it a separate App
// (`docs/adr/0022` §3).
plexus_service!(SettingsSvc => dyn crate::settings::Settings, "tui-settings", Seam, "The settings a launcher can read and change");

// The providers, on the same terms and for the same reason: which accounts
// exist, what a protocol is called, and what writing one back to the
// configuration means are the product's answers. This one carries a credential
// on its way *in* and never on its way out — see `crate::providers`.
plexus_service!(ProvidersSvc => dyn crate::providers::Providers, "tui-providers", Seam, "The provider accounts and models a launcher can read and change");
// And the plugins, on the same terms: which marketplaces are registered and
// what they carry is the product's answer, and every change to it is a `git`
// this crate must not know how to run — see `crate::plugins`.
plexus_service!(PluginsSvc => dyn crate::plugins::Plugins, "tui-plugins", Seam, "The plugins and marketplaces a launcher can read and change");
// And the tool catalog, on the same terms: what the model can call right now is
// a fact of the running tree, which this crate may not reach into
// (`docs/adr/0022` §3) — so it arrives over a seam like everything else.
plexus_service!(ToolCatalogSvc => dyn crate::tools::Tools, "tui-tools", Seam, "The tool catalog a person can look at and switch, one tool at a time");
// And the MCP servers, on the same terms and for the same reason: which servers
// exist, whether this project is trusted, and what changing one means are the
// running tree's answers, and this crate may not reach into it (`docs/adr/0022`
// §3). The name here is the one the launcher's row declares in its `provides()`.
plexus_service!(McpSvc => dyn crate::mcp::Mcp, "tui-mcp", Seam, "The MCP servers a person can look at, drill into and change");
plexus_service!(RewindSvc => dyn crate::rewind::Rewind, "tui-rewind", Seam, "The turns this session can be taken back to, and the taking back");
// Throwing a stored session away: the store is on disk and this crate does not
// reach disks (`docs/adr/0022` §3), so the panel asks over a seam.
plexus_service!(ResumeSvc => dyn crate::resume::Resume, "tui-resume-store", Seam, "Throwing away a stored session the resume panel lists");
// The directories a person marked to come back to: kept in the launcher's
// configuration file, which this crate does not read.
plexus_service!(PlacesSvc => dyn crate::places::Places, "tui-places", Seam, "The directories a person marked for `/cd` to offer first");
// And the seed installation, on the same terms: unpacking the embedded seeds,
// scanning the project and locking a file are the launcher's to do — this crate
// keeps its `atomcode-capabilities` features down to `tools` on purpose, and
// `/setup` on a brand-new machine is the one thing that needs more than that.
plexus_service!(SetupSvc => dyn crate::setup::Setup, "tui-setup", Seam, "Installing the seed skills on a machine that has none yet");

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

impl SessionView {
    fn settled(&self) -> bool {
        self.outstanding.is_empty() && matches!(self.status, None | Some(AgentStatus::Idle))
    }
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
        views.screen().is_none_or(SessionView::settled)
    }

    /// [`settled`](Self::settled), for the session this screen follows rather
    /// than the one on screen — they differ while a member is looked at, and
    /// quitting is about the lead's turn whichever is up.
    pub fn root_settled(&self) -> bool {
        let views = self.view.lock().expect("client poisoned");
        views
            .sessions
            .get(&views.root)
            .is_none_or(SessionView::settled)
    }

    /// What `session` last said it was doing, when it has said.
    pub(crate) fn status_of(&self, session: &str) -> Option<AgentStatus> {
        self.view
            .lock()
            .expect("client poisoned")
            .sessions
            .get(session)
            .and_then(|view| view.status)
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

/// What is on disk under `cwd` matching `prefix`, for the `@` menu.
///
/// IO, and deliberately here rather than in a module: a module may not touch
/// the world (`gates/tui-layers.sh`), and the composer's menu is filled by the
/// loop, which already reads the environment for the working directory.
///
/// A directory is listed with its separator so the next keystroke continues
/// into it. Bounded, because a repository root can hold thousands of entries
/// and a menu is a hint, not a file manager.
fn paths_under(cwd: &str, prefix: &str) -> Vec<crate::menu::Item> {
    const MOST: usize = 20;
    let (dir, leaf) = match prefix.rsplit_once('/') {
        Some((dir, leaf)) => (dir.to_string(), leaf.to_string()),
        None => (String::new(), prefix.to_string()),
    };
    let root = std::path::Path::new(cwd).join(&dir);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<crate::menu::Item> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // A dot file only when the typist asked for one: `@` in a repository
        // root would otherwise open with `.git` and `.gitignore`.
        if name.starts_with('.') && !leaf.starts_with('.') {
            continue;
        }
        if !name.starts_with(&leaf) {
            continue;
        }
        let folder = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let shown = if dir.is_empty() {
            name.clone()
        } else {
            format!("{dir}/{name}")
        };
        let value = if folder {
            format!("@{shown}/")
        } else {
            format!("@{shown}")
        };
        let item = crate::menu::Item::new(value.clone(), value);
        out.push(if folder {
            item.about(t(Msg::MenuFolder).into_owned())
        } else {
            item
        });
    }
    // By what is shown, which is what a person scans.
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out.truncate(MOST);
    out
}

/// The `/effort` rows the menu expands to for a given model: each level the
/// model exposes, then `default` (leave it to the endpoint). An EMPTY `levels`
/// — a model with no reasoning-effort control — yields only `default`, which is
/// the whole point of reading them from the description rather than a fixed set.
///
/// The order is the description's (already canonical); `default` is last so the
/// "no opinion" row sits at the bottom, under the concrete levels.
fn effort_menu_options(levels: &[String]) -> Vec<crate::command::CommandOption> {
    levels
        .iter()
        .map(|level| crate::command::CommandOption::new(level.clone(), t(Msg::EffortAbout)))
        .chain(std::iter::once(crate::command::CommandOption::new(
            "default",
            t(Msg::EffortDefaultAbout),
        )))
        .collect()
}

/// Lines the conversation moves per wheel notch.
///
/// One, not three. The terminal already sends one event per notch, so three
/// lines an event is three times the distance the hand asked for — and the
/// reason to reach for the wheel here is to study something that went past,
/// which is exactly when precision beats speed.
const WHEEL_LINES: i32 = 1;

/// How close two presses on the same cell must be to count as a double- (then
/// triple-) click. 400ms is the common desktop default — long enough for a
/// deliberate second tap, short enough that two separate clicks are not fused.
const MULTI_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

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
            // The four words are the product's: the other front end offers the
            // same four ways out of the same intervention, and two wordings for
            // one choice is how a person comes to think they are two choices.
            A::CompleteExternally => Some(("done", pt(PMsg::PolicyRecoveryComplete))),
            A::SkipStep => Some(("skip", pt(PMsg::PolicyRecoverySkip))),
            A::ViewSafeInstructions => Some(("how", pt(PMsg::PolicyRecoveryInstructions))),
            A::EndTask => Some(("end", pt(PMsg::PolicyRecoveryEnd))),
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

/// Put each prompt on the composer's line and hand back what the person typed.
///
/// Split from [`serve_askpass`] so it can be judged without a socket: what is
/// worth judging is that a prompt reaches the field, that the answer reaches the
/// one waiting for it, and that esc reaches them as a refusal.
#[cfg(unix)]
async fn answer_prompts(
    host: Arc<crate::host::Host>,
    wake: mpsc::UnboundedSender<Wake>,
    mut prompts: tokio::sync::mpsc::Receiver<atomcode_capabilities::askpass::server::AskpassPrompt>,
) {
    while let Some(prompt) = prompts.recv().await {
        // Not a modal: a password is typed, and what a person types belongs on
        // the line they type on — the same place `atomcode-tuix` asks for it.
        // The reply travels with it, so `None` reaches the asking program as a
        // refusal however the prompt ends. See `crate::secret`.
        host.ask_secret(&prompt.prompt, prompt.reply);
        // The prompt arrives while nothing else is waking the loop: a turn is
        // running and the screen is idle between frames.
        if wake.send(Wake::Fact).is_err() {
            break;
        }
    }
}

/// The least time between two questions about the allowance.
///
/// The figure only moves when a turn spends something, and the question costs a
/// round trip on the account — so it is asked after a turn and then held off
/// for this long. The number is the one `atomcode-tuix` settled on.
pub const ALLOWANCE_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

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
#[derive(Clone)]
struct Member {
    name: String,
    status: AgentStatus,
    /// Turns it has opened while watched.
    turns: u64,
    /// Stopped and gone from the registry: kept, so it can still be looked at.
    gone: bool,
}

/// Every member this screen has heard of, stopped ones included.
///
/// A trait, and a seam, because `/agents` lists them: a command reaches the
/// tree's services rather than the screen (`docs/adr/0021`), and the roster is
/// the screen's, so the screen is what hands it over. The lead is not one of
/// them — it is the session itself.
pub trait TeamRoster: Send + Sync {
    /// Name, session id, and whether it has stopped, in name order.
    fn members(&self) -> Vec<(String, String, bool)>;
}

plexus_service!(TeamRosterSvc => dyn TeamRoster, "tui-team-roster", Seam, "Every member this screen has heard of, stopped ones included");

/// The members, by session id, as the connection's events described them.
///
/// Shared behind a lock rather than folded into the `Moment`, because the panel
/// and `/agents` ask different questions of it: the panel wants what is running
/// *now*, and this wants everything that has ever been on the team. The moment
/// holds the first — see `refresh_members` — and this holds both.
#[derive(Default)]
pub struct Roster(Mutex<BTreeMap<String, Member>>);

impl Roster {
    fn note(&self, session: &str, member: Member) {
        self.0
            .lock()
            .expect("roster poisoned")
            .insert(session.to_string(), member);
    }
    /// It stopped. The entry stays: its log is still there to read, and
    /// `/agents` is how a person reaches it (`docs/adr/0023` §5).
    fn mark_gone(&self, session: &str) -> bool {
        self.0
            .lock()
            .expect("roster poisoned")
            .get_mut(session)
            .map(|member| member.gone = true)
            .is_some()
    }
    /// Running a turn right now — and not stopped, since a member that has
    /// stopped runs nothing.
    fn is_working(&self, session: &str) -> bool {
        self.0
            .lock()
            .expect("roster poisoned")
            .get(session)
            .is_some_and(|member| member.status != AgentStatus::Idle && !member.gone)
    }
    /// Its status moved. A member that goes to working has opened a turn.
    /// `true` when there was such a member.
    fn set_status(&self, session: &str, status: AgentStatus) -> bool {
        match self.0.lock().expect("roster poisoned").get_mut(session) {
            Some(member) => {
                if status == AgentStatus::Working && member.status != AgentStatus::Working {
                    member.turns += 1;
                }
                member.status = status;
                true
            }
            None => false,
        }
    }
    /// Every member, as `MemberNow` — what the team panel is drawn from.
    fn moment_members(&self) -> Vec<crate::moment::MemberNow> {
        use crate::moment::{Activity, MemberNow};
        let mut out: Vec<MemberNow> = self
            .0
            .lock()
            .expect("roster poisoned")
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
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
    fn clear(&self) {
        self.0.lock().expect("roster poisoned").clear();
    }
    /// A member, for a test that wants one without driving a whole connection
    /// through `AgentAdded`.
    #[cfg(test)]
    pub(crate) fn note_for_test(&self, session: &str, name: &str, gone: bool) {
        self.note(
            session,
            Member {
                name: name.to_string(),
                status: AgentStatus::Idle,
                turns: 0,
                gone,
            },
        );
    }
}

impl TeamRoster for Roster {
    fn members(&self) -> Vec<(String, String, bool)> {
        let mut out: Vec<(String, String, bool)> = self
            .0
            .lock()
            .expect("roster poisoned")
            .iter()
            .map(|(session, member)| (member.name.clone(), session.clone(), member.gone))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// The assembled UI. Public so a test can drive exactly what ships.
pub struct Tui {
    client: Arc<AgentClient>,
    host: Arc<Host>,
    keys: Arc<Keys>,
    surface: Arc<dyn Surface>,
    /// Set when the loop starts. A command runs against the tree, and the tree
    /// is not known until then.
    ctx: Mutex<Option<Context>>,
    wake: Mutex<Option<mpsc::UnboundedSender<Wake>>>,
    /// When the allowance was last asked about, so it is not asked again for
    /// [`ALLOWANCE_EVERY`]. `None` until the first turn ends.
    allowance_checked: Mutex<Option<std::time::Instant>>,
    /// Where the button went down, so a release can tell a click from a drag.
    pressed_at: Mutex<Option<(u16, u16)>>,
    /// The last press's time, cell, and how many presses have landed on that
    /// cell in a row — so a second press within the window is a double-click
    /// (word) and a third a triple-click (line). The app reproduces what taking
    /// the mouse for drag-select took from the terminal.
    click_streak: Mutex<Option<ClickStreak>>,
    /// The session's members, by session id — stopped ones included, so
    /// `/agents` can still reach their logs. Shared with the tree as an
    /// `Arc`, because that seam is how a command reads it.
    members: Arc<Roster>,
    /// What this screen last told the terminal the window is called, so a
    /// frame that changes nothing writes nothing.
    named: Mutex<Option<String>>,
    /// Where an attached image's bytes were written on disk, by marker number, so
    /// a person clicking `[Image #N]` opens it in their desktop viewer. Written
    /// on first open and reused after — the picture is decoded once, not on every
    /// click. Session-scoped, like the gallery it mirrors.
    image_files: Mutex<std::collections::HashMap<usize, std::path::PathBuf>>,
}

#[async_trait]
impl UserInterface for Tui {
    fn describe(&self) -> String {
        format!("full-screen terminal on {}", self.surface.describe())
    }

    /// Run a slash command the way a person typing it would — the screen's own
    /// dispatch, with its own context and its own delivery of what it said.
    ///
    /// A no-op before the loop is up: there is no tree to run against yet,
    /// which is the honest answer rather than a queued one.
    fn run_slash(&self, line: &str) {
        self.run_command(line);
    }

    /// A line the person should read, into the conversation.
    ///
    /// The same block a command's answer becomes (`CommandSaid`), so it scrolls,
    /// folds and is dumped on exit with everything else. That is what lets a row
    /// talk *while* it works — a login's QR code, URL and each step — instead of
    /// holding it all for a modal the person has to read before it closes.
    fn say(&self, text: &str) {
        Tui::say(self, text);
        // Written from off the loop, so ask for the frame the same way anything
        // else outside it does (`deliver` does the same after a command
        // answers): without this the line sits there unpainted until the next
        // keystroke.
        let sender = self.wake.lock().expect("wake poisoned").clone();
        if let Some(sender) = sender {
            let _ = sender.send(Wake::Fact);
        }
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
        // Kept before the connection takes it: the one question asked of the
        // host before a person has typed anything.
        let readiness = control.clone();
        // And the second one — which mode this session is in, which a host may
        // have set before this screen subscribed.
        let mode_of_session = control.clone();
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

        // The settings rows, read once here: the title is written from
        // `name_the_window`, which runs every frame, and a row read per frame
        // would be a configuration file read per frame. Once at the start, and
        // again after every change the panel makes (`run_settings_key`), is
        // enough for the light — unlike the per-frame read, which would be
        // paying for a file nobody has necessarily touched.
        self.refresh_settings();

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
        // loop below rather than here, because the welcome names the session's
        // model, which arrives asynchronously as the `Described` off the
        // subscription just sent (`follow` above) — there is nothing to say yet
        // at this point.
        //
        // The stream is emission-ordered (`block.rs`: positions are assigned on
        // open and never move), so the welcome sits on top only by being emitted
        // before any history. The feed's order (`harness/src/feed.rs`) makes that
        // reachable: on `Subscribe` it sends `Described` first, then the status,
        // the members, and only then the facts, one wake at a time. So the loop
        // emits the welcome the moment `described()` first answers — into a
        // still-empty stream — and the backfill folds in beneath it over the
        // wakes that follow. Waiting for a drained queue instead would fold the
        // history first and leave the welcome stranded at the bottom.
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

        // Before anything is typed: would a turn be taken at all?
        //
        // The gap this closes: this front end used to find out that there is no
        // provider by submitting a turn and getting an error back — which tells
        // the person after they have written one. The old driver protocol had
        // three pre-flight checks for exactly this and the bridge to this
        // screen never carried them over; the contract asks once instead, and
        // the answer carries what to do about it.
        {
            let session = session.clone();
            let host = self.host.clone();
            let keys = wake_tx.clone();
            tokio::spawn(async move {
                let Ok(HostReply::Readiness {
                    ready: false,
                    why,
                    fix,
                }) = readiness.call(HostCommand::Readiness { session }).await
                else {
                    return;
                };
                if let Some(why) = why {
                    let mut stream = host.stream.write().expect("stream poisoned");
                    let mut w = stream.writer("commands");
                    w.emit(
                        crate::block::Coord::default(),
                        Arc::new(crate::content::NoticeBlock { detail: why }),
                    );
                    drop(stream);
                    let _ = keys.send(Wake::Fact);
                }
                // Dispatched the way a modal's pick is: the host names one of
                // this screen's own commands, and naming it is as far as its
                // say goes.
                if let Some(fix) = fix {
                    let _ = keys.send(Wake::Chose(Some(fix)));
                }
            });
        }

        // How much this session may do without asking, asked once before the
        // events start. The mode can be set before this screen subscribed — a
        // host's own `--dangerously-skip-permissions` seeds it at startup — and
        // an event pushed to nobody is an event nobody heard. So the read
        // happens here and `HostEvent::ModeChanged` carries it from then on,
        // which is the same bargain as the readiness question above and the
        // autonomy line: the one thing that must not happen is a session
        // running unattended while the row says it is an ordinary one.
        {
            let announced = session.clone();
            let control = mode_of_session;
            let host = self.host.clone();
            let keys = wake_tx.clone();
            tokio::spawn(async move {
                let Ok(HostReply::Mode { mode }) =
                    control.call(HostCommand::Mode { session }).await
                else {
                    return;
                };
                // Through the same filter the pushed news takes: what arrived
                // is about the session this screen follows, and `took_mode` is
                // where that is decided.
                if took_mode(&host.moment, &announced, mode) {
                    let _ = keys.send(Wake::Fact);
                }
            });
        }

        if let Some(text) = initial {
            // A start-up prompt is text by construction — there is no composer
            // yet, so there is nothing it could have been attached to. Kept as
            // `last_sent` all the same, so an Escape that stops this first turn
            // hands it back the way it would any other (see `Action::Escape`).
            self.host.moment.write().expect("moment poisoned").last_sent = Some(text.clone());
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
            // **Three** things follow the pointer now, and any one of them is a
            // reason for the terminal to report every cell it crosses: the
            // composer's menu, a question on screen — where the row under the
            // pointer is the row a click would take — and the slash menu, which
            // lights the row the pointer is over for the same reason.
            //
            // This is a request to the terminal, not a redraw: it changes what
            // the terminal *sends*, not what is on the screen, which is why it
            // is not folded into `stale`.
            self.surface.set_motion(
                self.host.context_menu_open()
                    || self.host.menu_open()
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
            // Asked the instant the session is described, **not** once the queue
            // is drained. The stream is append-ordered (`block.rs`), so the
            // welcome has to be emitted before the history to sit above it — and
            // the feed's order makes that safe: on `Subscribe` it sends
            // `Described` first and then, one wake at a time, the facts. So the
            // turn after `described()` first answers, `open_conversation` finds a
            // still-empty stream and lands the welcome on top; the backfill folds
            // in below it over the wakes that follow. `described()` is reset by
            // `follow` and re-answered per session, so a resume waits for the
            // resumed session's own description rather than the outgoing one's.
            {
                if owes_opening {
                    if let Some(described) = client.described() {
                        let cwd = self
                            .host
                            .moment
                            .read()
                            .expect("moment poisoned")
                            .cwd
                            .clone();
                        let open =
                            crate::module::Opening {
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
                                // Resolved **here**, not when the welcome producer
                                // mounted: the launcher's rows mount after the
                                // screen's, so a lookup at mount time would be empty
                                // for the one launcher that has words to provide.
                                // This runs after the whole tree is up.
                                words: self.ctx.lock().expect("ctx poisoned").as_ref().and_then(
                                    |ctx| ctx.service::<crate::plugin::WelcomeWordsSvc>(),
                                ),
                            };
                        stale |= self
                            .host
                            .open_conversation(crate::block::Coord::default(), &open);
                        // Answered once per session, whatever the answer: a
                        // stream that was not empty will not become empty
                        // again, and one that opened is no longer empty. It is
                        // *per session* rather than once in a lifetime because
                        // the `SessionChanged` arm above raises it again — the
                        // one thing that can empty the stream under this loop.
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
                        self.members.clear();
                        self.host.switch_session();
                        client.follow(&session);
                        {
                            let mut m = self.host.moment.write().expect("moment poisoned");
                            m.lead = session.clone();
                            m.viewing = session.clone();
                        }
                        // The session that arrives owes its own first word.
                        // `switch_session` empties the stream (`host.rs`
                        // `switch_view`), so the question below is live again —
                        // and it is asked, not answered, once per session. Left
                        // down, a session started with `/clear` opened bare: the
                        // welcome block is produced by `open_conversation` alone,
                        // and nothing else asks.
                        owes_opening = true;
                        self.host.say(
                            t(Msg::SwitchedToSession { session: &session }).into_owned(),
                            false,
                        );
                    }
                    stale = true;
                }
                // What the session is doing on its own. Kept, not said: a
                // round landing is not news to read, it is a number to watch,
                // and saying it every round would bury the conversation under
                // its own progress bar.
                Wake::Host(HostEvent::Autonomy { session, running }) => {
                    stale |= took_autonomy(&self.host.moment, &session, running);
                }
                // How much the session may do without asking. Kept, not said:
                // the mode is drawn on the status row, and a line of prose per
                // change would put a sentence in the conversation for a badge
                // that already says it — a person's own Shift+Tab would read as
                // news.
                Wake::Host(HostEvent::ModeChanged { session, mode }) => {
                    stale |= took_mode(&self.host.moment, &session, Some(mode));
                }
                // The turn finished; the log did not get it. Said loudly and
                // at once: the log is the session's only authority
                // (`docs/adr/0024`), so a person who is not told now will find
                // out by resuming tomorrow into a conversation missing a turn.
                Wake::Host(HostEvent::PersistenceFailed { message, .. }) => {
                    self.host.say(
                        t(Msg::TurnNotStored { message: &message }).into_owned(),
                        true,
                    );
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
                //
                // Every thing that asked for motion has to be listed here, or the
                // hover it asked for is read as a terminal taking the mouse back:
                // the slash menu is the fourth such thing, and it is the one that
                // is up while a person is typing — the moment a spurious "the
                // terminal took your mouse" notice would be most visible.
                Wake::Input(Input::Mouse(Click::Hover, ..))
                    if !self.host.context_menu_open()
                        && !self.host.menu_open()
                        && !self.host.asks.is_waiting()
                        && !self.team_on_screen() =>
                {
                    self.surface.heal_mouse();
                    self.host
                        .say(t(Msg::MouseTakenBackAuto).into_owned(), false);
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
                    // The slash menu, before anything else looks at the pointer.
                    // A list with a lit row is something a press can choose from
                    // and a pointer can travel over, and both of those are the
                    // menu's while it is up. A press that lands *off* it is not
                    // swallowed: it falls through, and the composer recomputes
                    // the menu from whatever the press did.
                    if matches!(click, Click::Press) {
                        if let Some(name) = self.host.menu_click(x, y) {
                            quit = self.take_command(&name, &client);
                            stale = true;
                            continue;
                        }
                    }
                    if matches!(click, Click::Hover) && self.host.menu_open() {
                        stale |= self.host.menu_hover(x, y);
                    }
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
                    // The wheel over the settings panel is the panel's: its
                    // read-only pages can be longer than it is, and a wheel
                    // that scrolled the conversation *behind* an open panel
                    // would be a wheel that does nothing a person can see.
                    if matches!(click, Click::WheelUp | Click::WheelDown) {
                        let by = match click {
                            Click::WheelUp => -WHEEL_LINES,
                            _ => WHEEL_LINES,
                        };
                        if self.host.settings_wheel(x, y, by) {
                            stale = true;
                            continue;
                        }
                        // And over the providers panel, which is a list longer
                        // than the rows it is given.
                        if self.host.plugins_wheel(x, y, by) {
                            stale = true;
                            continue;
                        }
                        if self.host.tools_wheel(x, y, by) {
                            stale = true;
                            continue;
                        }
                        if self.host.rewind_wheel(x, y, by) {
                            stale = true;
                            continue;
                        }
                        if self.host.resume_wheel(x, y, by) {
                            stale = true;
                            continue;
                        }
                        if self.host.providers_wheel(x, y, by) {
                            stale = true;
                            continue;
                        }
                    }
                    let action = match click {
                        Click::WheelUp => Some(Action::Scroll(-WHEEL_LINES)),
                        Click::WheelDown => Some(Action::Scroll(WHEEL_LINES)),
                        Click::Press => {
                            *self.pressed_at.lock().expect("press poisoned") = Some((x, y));
                            // How many presses have landed on this cell in a
                            // row: a second within the window is a word, a third
                            // a line. Recorded on every press so the run is
                            // right even when a press is consumed as chrome below.
                            let clicks = {
                                let now = std::time::Instant::now();
                                let mut streak = self.click_streak.lock().expect("streak poisoned");
                                let n = match *streak {
                                    Some((at, cell, n))
                                        if cell == (x, y)
                                            && now.duration_since(at) <= MULTI_CLICK =>
                                    {
                                        // Saturating: a stuck/auto-repeating button
                                        // must not overflow (panic in debug) — and
                                        // we only distinguish 1 / 2 / ≥3 anyway.
                                        n.saturating_add(1)
                                    }
                                    _ => 1,
                                };
                                *streak = Some((now, (x, y), n));
                                n
                            };
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
                            // The row is what the press took — not "whatever
                            // was focused" — so the pointer's own row is read
                            // here, and the keyboard is handed back either way:
                            // the press has been answered.
                            if let Some(row) = self.host.team_row_at(x, y) {
                                let _ = self.host.point_team_at(row);
                                if let Some(session) = self.host.team_target() {
                                    self.switch_to(&session);
                                }
                                self.host.unfocus_team();
                                stale = true;
                                continue;
                            }
                            // A press on a page tab shows that page. Tried
                            // before the settings rows, because the tabs are on
                            // the header row and a hit test that ran the other
                            // way round would let a stray row answer first.
                            //
                            // Deliberately **not** falling through to the
                            // selection when it misses: a press on the header is
                            // a press on the panel's chrome, and starting a text
                            // selection on a tab row would copy the panel's own
                            // labels.
                            if self.host.settings_open() {
                                if let Some(tab) = self.host.settings_tab_at(x, y) {
                                    let _ = self.host.show_settings_tab(tab);
                                    stale = true;
                                    continue;
                                }
                                // And on the inner row, when the page has one.
                                // Tried here for the same reason the outer row
                                // is: it is chrome on a row of its own, and a
                                // hit test that ran the settings rows first
                                // would hand it to whatever the list put there.
                                if let Some(page) = self.host.settings_stats_page_at(x, y) {
                                    let _ = self.host.show_stats_page(page);
                                    stale = true;
                                    continue;
                                }
                            }
                            // The plugins panel's header and rows, on the same
                            // terms: chrome first, so a stray row cannot answer a
                            // press aimed at a tab.
                            if self.host.plugins_open() {
                                if let Some(tab) = self.host.plugins_tab_at(x, y) {
                                    let _ = self.host.show_plugins_tab(tab);
                                    stale = true;
                                    continue;
                                }
                                if let Some(row) = self.host.plugins_row_at(x, y) {
                                    let _ = self.host.point_plugins_at(row);
                                    self.run_plugins_key(crate::surface::KeyPress::plain(
                                        crate::surface::Key::Enter,
                                    ));
                                    stale = true;
                                    continue;
                                }
                            }
                            // And the tools panel: a click on a row points at
                            // it and throws its switch, which is the whole of
                            // what this panel does.
                            if self.host.tools_open() {
                                if let Some(row) = self.host.tools_row_at(x, y) {
                                    let _ = self.host.point_tools_at(row);
                                    self.run_tools_key(crate::surface::KeyPress::plain(
                                        crate::surface::Key::Enter,
                                    ));
                                    stale = true;
                                    continue;
                                }
                            }
                            // And the MCP panel: a click points at the row, and
                            // only a second click on the **same level and row**
                            // presses Enter — the same discipline the keyboard
                            // has, since the actions down here include 停用 and
                            // 登出. The level matters as much as the row: going
                            // in and out of a server puts different things under
                            // the same cell, so comparing rows alone would let
                            // the second half of a double click land on an
                            // action that was not on screen for the first half.
                            if self.host.mcp_open() {
                                if let Some(row) = self.host.mcp_row_at(x, y) {
                                    if self.host.mcp_click(row) {
                                        self.run_mcp_key(crate::surface::KeyPress::plain(
                                            crate::surface::Key::Enter,
                                        ));
                                    }
                                    stale = true;
                                    continue;
                                }
                            }
                            // And the rewind panel: a click on a turn points at
                            // it and walks on to the second step — the same two
                            // presses the keyboard makes, so a click can never
                            // reach further than a key can.
                            if self.host.rewind_open() {
                                if let Some(row) = self.host.rewind_row_at(x, y) {
                                    let _ = self.host.point_rewind_at(row);
                                    self.run_rewind_key(crate::surface::KeyPress::plain(
                                        crate::surface::Key::Enter,
                                    ));
                                    stale = true;
                                    continue;
                                }
                            }
                            // And the resume panel: a click on a session points
                            // at it and resumes it, the same as Enter on the row.
                            if self.host.resume_open() {
                                if let Some(row) = self.host.resume_row_at(x, y) {
                                    let _ = self.host.point_resume_at(row);
                                    self.run_resume_key(crate::surface::KeyPress::plain(
                                        crate::surface::Key::Enter,
                                    ));
                                    stale = true;
                                    continue;
                                }
                            }
                            // The providers panel's header, on the same terms
                            // as the settings one: chrome first, so a stray row
                            // cannot answer a press aimed at a tab.
                            if self.host.providers_open() {
                                if let Some(tab) = self.host.providers_tab_at(x, y) {
                                    let _ = self.host.show_providers_tab(tab);
                                    stale = true;
                                    continue;
                                }
                                // And a press on one of its rows takes that row,
                                // through the return key's own path — so a click
                                // that walks into an account and a keypress that
                                // does cannot come to mean different things.
                                if let Some(row) = self.host.providers_row_at(x, y) {
                                    let _ = self.host.point_providers_at(row);
                                    self.run_providers_key(crate::surface::KeyPress::plain(
                                        crate::surface::Key::Enter,
                                    ));
                                    stale = true;
                                    continue;
                                }
                            }
                            // A press on a settings row arms it and takes it,
                            // which is the rule the question and team panels
                            // keep: the row a press lands on is the row that
                            // acts, never a hidden default. It goes through the
                            // return key's own path rather than a second
                            // "confirm the pointed row", so a click and a
                            // keypress cannot come to mean different things.
                            if self.host.settings_open() {
                                if let Some(row) = self.host.settings_row_at(x, y) {
                                    let _ = self.host.point_settings_at(row);
                                    // The key's own path, so a click and a
                                    // keypress cannot come to mean different
                                    // things. Either way a frame is owed: the
                                    // press moved the highlight even when the
                                    // setting did not move.
                                    self.run_settings_key(crate::surface::KeyPress::plain(
                                        crate::surface::Key::Enter,
                                    ));
                                    stale = true;
                                    continue;
                                }
                            }
                            match clicks {
                                // Forget the press so the release does not become
                                // a `ClickAt` — which, not being a selection
                                // gesture, would clear the word/line we just put
                                // up. The gesture already copied on this press;
                                // the highlight stays, like a drag's does.
                                2 => {
                                    *self.pressed_at.lock().expect("press poisoned") = None;
                                    Some(Action::SelectWord(x, y))
                                }
                                n if n >= 3 => {
                                    *self.pressed_at.lock().expect("press poisoned") = None;
                                    Some(Action::SelectLine(x, y))
                                }
                                _ => Some(Action::SelectFrom(x, y)),
                            }
                        }
                        Click::Drag => Some(Action::SelectTo(x, y)),
                        Click::Release => {
                            let from = self.pressed_at.lock().expect("press poisoned").take();
                            match from {
                                Some(p) if p == (x, y) => Some(Action::ClickAt(x, y)),
                                Some(_) => Some(Action::CopySelection),
                                // No press to release against: a multi-click
                                // cleared it. Copy whatever is selected (a word,
                                // a line, or a double-click-then-drag extension)
                                // — a no-op when nothing is. The highlight stays.
                                None => Some(Action::CopySelection),
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
                            // And the settings panel, for the same reason: the
                            // row under the pointer is the row a press would arm,
                            // and a highlight somewhere else while the pointer is
                            // somewhere is the panel lying about its own state.
                            if self.host.settings_open() {
                                if let Some(row) = self.host.settings_row_at(x, y) {
                                    stale |= self.host.point_settings_at(row);
                                }
                            }
                            // And the providers panel.
                            if self.host.providers_open() {
                                if let Some(row) = self.host.providers_row_at(x, y) {
                                    stale |= self.host.point_providers_at(row);
                                }
                            }
                            // And the plugins panel.
                            if self.host.plugins_open() {
                                if let Some(row) = self.host.plugins_row_at(x, y) {
                                    stale |= self.host.point_plugins_at(row);
                                }
                            }
                            // And the tools panel.
                            if self.host.tools_open() {
                                if let Some(row) = self.host.tools_row_at(x, y) {
                                    stale |= self.host.point_tools_at(row);
                                }
                            }
                            // And the MCP panel, for the same reason: the row under
                            // the pointer is the row a press would take, and a
                            // highlight somewhere else while the pointer is somewhere
                            // is the panel lying about its own state.
                            if self.host.mcp_open() {
                                if let Some(row) = self.host.mcp_row_at(x, y) {
                                    stale |= self.host.point_mcp_at(row);
                                }
                            }
                            // And the rewind panel.
                            if self.host.rewind_open() {
                                if let Some(row) = self.host.rewind_row_at(x, y) {
                                    stale |= self.host.point_rewind_at(row);
                                }
                            }
                            if self.host.resume_open() {
                                if let Some(row) = self.host.resume_row_at(x, y) {
                                    stale |= self.host.point_resume_at(row);
                                }
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
                // A password being asked for takes a paste before the composer
                // does, and this is why the arm is here rather than beside the
                // key one: without it a pasted password would land in the
                // draft, which is history, and which becomes a `UserMessage`
                // fact the moment it is sent. A password manager's paste is how
                // most people answer a prompt like this.
                Wake::Input(Input::Paste(text)) if self.host.secret_waiting() => {
                    self.host.secret_paste(&text);
                    stale = true;
                }
                // A paste while the providers panel is up belongs to the field
                // it is working in: the composer is off screen, so falling
                // through would be typing into something nobody can see.
                Wake::Input(Input::Paste(text)) if self.host.providers_open() => {
                    stale |= self.host.providers_paste(&text);
                }
                // And a paste while the plugins panel is up: a marketplace
                // address is exactly the thing that arrives by paste, and
                // falling through would put it in a composer nobody can see.
                Wake::Input(Input::Paste(text)) if self.host.plugins_open() => {
                    stale |= self.host.plugins_paste(&text);
                }
                // And a paste while the tools panel is up: a tool name is
                // exactly the thing that arrives by paste.
                Wake::Input(Input::Paste(text)) if self.host.tools_open() => {
                    stale |= self.host.tools_paste(&text);
                }
                // And a paste while the MCP panel is up, on the same terms: a
                // server name is exactly the thing that arrives by paste, and
                // falling through would put it in a composer nobody can see.
                Wake::Input(Input::Paste(text)) if self.host.mcp_open() => {
                    stale |= self.host.mcp_paste(&text);
                }
                Wake::Input(Input::Paste(text)) => {
                    quit = self.act(Action::Paste(text), &client);
                    stale = true;
                }
                // A password a blocked process is waiting on has the keyboard
                // before anything else — above a modal, because it is not one:
                // it is the field, it is one key from being refused, and a
                // `sudo` is holding the turn open until it is. Everything else
                // is swallowed rather than passed on, so a key cannot act on a
                // screen whose field is showing something else.
                Wake::Input(Input::Key(press)) if self.host.secret_waiting() => {
                    self.host.secret_key(press);
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
                // The settings panel, while it is up. Above the question and
                // above the ordinary bindings for the reason the modal is: it
                // was opened deliberately, and what it is for is being worked in
                // right now. Below the modal, because a modal is a question the
                // screen cannot answer for the person.
                Wake::Input(Input::Key(press)) if self.host.settings_open() => {
                    stale |= self.run_settings_key(press);
                }
                // The providers panel, on the same terms: only one of the two is
                // ever up (`Host::toggle_providers`), so these two arms cannot
                // both match.
                Wake::Input(Input::Key(press)) if self.host.providers_open() => {
                    stale |= self.run_providers_key(press);
                }
                // And the plugins panel, on the same terms: at most one of the
                // three is ever up (`Host::toggle_plugins`).
                Wake::Input(Input::Key(press)) if self.host.plugins_open() => {
                    stale |= self.run_plugins_key(press);
                }
                // And the tools panel, on the same terms: at most one of the
                // four is ever up (`Host::toggle_tools`).
                Wake::Input(Input::Key(press)) if self.host.tools_open() => {
                    stale |= self.run_tools_key(press);
                }
                // And the MCP panel, on the same terms: at most one of the family
                // is ever up (`Host::toggle_mcp`).
                Wake::Input(Input::Key(press)) if self.host.mcp_open() => {
                    stale |= self.run_mcp_key(press);
                }
                // And the rewind panel, on the same terms: at most one of the
                // five is ever up (`Host::toggle_rewind`). Above the composer
                // for the reason all of them are — and it is also why a person
                // in this panel can press Esc without it reaching the
                // double-tap in `act`: the panel owns the key while it is up,
                // and Esc in there means "back a step", then "put it away".
                Wake::Input(Input::Key(press)) if self.host.rewind_open() => {
                    stale |= self.run_rewind_key(press);
                }
                // And the resume panel, on the same terms: at most one of the
                // panels is ever up, and the one that is owns the keys — so Esc
                // in here means "put it away", not the composer's double-tap.
                Wake::Input(Input::Key(press)) if self.host.resume_open() => {
                    stale |= self.run_resume_key(press);
                }
                // A question on screen gets first refusal on every key. It is a
                // panel riding the tail now, not a modal, so this is the only
                // place its keys are routed — and focus is still arbitration,
                // not composition: exactly one thing can hold it.
                Wake::Input(Input::Key(press)) if self.host.asks.is_waiting() => {
                    quit = self.answer_question(press);
                    stale = true;
                }
                // The slash menu, above the ordinary bindings for the keys it
                // owns — and only those. It is a list with a lit row, so up/down
                // walk it, tab completes onto the line, and esc puts it away.
                // Everything else, **enter included**, stays the composer's: the
                // menu puts a command on the line and enter sends what is on the
                // line, which is what keeps `/effort` reporting the current
                // level and a half-typed `/comp` reaching the dispatcher that
                // suggests `/compact`.
                Wake::Input(Input::Key(press))
                    if self.host.menu_open() && crate::menu::Slash::owns(press) =>
                {
                    quit = self.slash_menu_key(press, &client);
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
                // Shift+Tab steps the execution mode on, and plain Tab too where
                // `ui.mode_switch_key = "tab"` says so — the phone, where
                // Shift+Tab is undeliverable. Above the composer's own keys
                // because it is *not* one: which key it is depends on a live
                // setting, so it cannot be a row in the keymap.
                //
                // After the two arms above, deliberately. Plain Tab is already
                // the completion menu's and the team panel's, and both of those
                // are about a list that is up or a panel that was opened on
                // purpose; the mode is not, so it yields to them. Shift+Tab is
                // nobody else's, which is why it is the default.
                Wake::Input(Input::Key(press)) if self.is_mode_cycle_key(press) => {
                    quit = self.cycle_mode();
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
        // And the password, for the same reason and with the same meaning: a
        // `sudo` waiting on an answer that is never coming holds the turn open,
        // and `None` is a refusal rather than a blank.
        self.host.refuse_secret();
        // The lead's turn is what the wait is for, whoever is on screen: the
        // turn events on this connection are the lead's, and a screen left on a
        // member used to stop only that member and then sit out the whole
        // timeout for a lead that was never asked. Quitting stops them all.
        let lead_busy = !client.root_settled();
        if lead_busy || !client.settled() {
            client.cancel_all();
        }
        if lead_busy {
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
        Ok(())
    }
}

impl Tui {
    fn paint(&self) {
        self.name_the_window();
        let frame = self.host.compose(self.surface.size());
        self.surface.present(&frame);
    }

    /// Put the session's name in the window title, when it has changed.
    ///
    /// Here rather than where the fact is folded, because the fact reaches a
    /// `Host` and the terminal belongs to the `Tui` — and because "when it
    /// changed" is a comparison against what this screen last said, which is
    /// this side's business. Costs one string compare a frame and writes
    /// nothing in the ordinary case.
    ///
    /// The title carries a status dot — `🟢` idle, `🟡` working, `🔴` waiting on
    /// the person (a question or approval) — then the session's name, or the
    /// project directory for a window not yet named. The dot is the "红绿灯" the
    /// reference and Claude Code put in the tab so a glance at the strip says
    /// which session wants you.
    ///
    /// **What the dot is comes from [`Host::light`], not from a test written
    /// here.** Two answers to "what is this session doing" is one too many, and
    /// the one over there is the one a person can turn off
    /// (`crate::settings::STATUS_DOT`) and the one the criteria are written
    /// against. This end composes the string.
    ///
    /// The light is read every frame rather than folded into a fact: it is true
    /// of *now* — a question arrives, a turn starts, a stop lands — and a title
    /// that only moved when the session was renamed would be a dot that lies
    /// about the one thing it is for. The compare below keeps that free: the
    /// light joins the string, so a frame that changed nothing writes nothing.
    fn name_the_window(&self) {
        // The light first, then the moment: asking it under the moment's own
        // read guard would take that lock twice on one thread, and an `RwLock`
        // with a writer queued between the two reads is a deadlock, not a
        // slowdown.
        let light = self.host.light();
        let (title, cwd) = {
            let m = self.host.moment.read().expect("moment poisoned");
            (m.title.clone(), m.cwd.clone())
        };
        // The project directory is the fallback for a window not yet named —
        // which checkout this is, when the session has no title of its own. At
        // the filesystem root there is no project segment, so fall back to the
        // app rather than a bare dot.
        let base = crate::text::basename(&cwd);
        let fallback = if base.is_empty() { "AtomCode" } else { base };
        let wanted = crate::text::terminal_title(
            title.as_deref(),
            fallback,
            light.map(crate::text::Light::dot),
        );
        if wanted.is_empty() {
            return;
        }
        let mut said = self.named.lock().expect("named poisoned");
        if said.as_deref() == Some(wanted.as_str()) {
            return;
        }
        self.surface.set_title(&wanted);
        *said = Some(wanted);
    }

    /// The session's members, as their events have told them, onto the moment.
    ///
    /// Nothing is folded for a member: a member is created, opens a turn and
    /// is stopped without this conversation committing a single fact, and it
    /// must stay that way — a member's log is its own. What the screen knows of
    /// one is what the connection pushed about it.
    /// Bring `Moment::settings` in step with what the launcher reads.
    ///
    /// Asked when the panel opens and after every change, never per frame: the
    /// port may read a file, and a `render` that did filesystem work would
    /// break the purity the whole crate rests on. Reading through the port is
    /// the only way to learn the rows — the screen has no other road to the
    /// configuration, which is what keeps it an App apart (`docs/adr/0022` §3).
    ///
    /// **True when the rows changed**, so a caller can tell "read them again"
    /// from "and they were different".
    fn refresh_settings(&self) -> bool {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return false;
        };
        // No port, no panel: a launcher that mounted the row without providing
        // `tui-settings` gets an empty list rather than a panic — the same
        // bargain every other absent seam strikes.
        let Some(port) = ctx.service::<crate::plugin::SettingsSvc>() else {
            return false;
        };
        let view = port.rows();
        let mut moment = self.host.moment.write().expect("moment poisoned");
        if moment.settings == view {
            return false;
        }
        moment.settings = view;
        true
    }

    /// Bring `Moment::providers` in step with what the launcher reads.
    ///
    /// Asked when the panel opens and after every write, never per frame, for
    /// the reason [`Tui::refresh_settings`] is: the port reads a file, and a
    /// `render` that did filesystem work would break the purity the crate rests
    /// on. **True when the rows changed.**
    fn refresh_providers(&self) -> bool {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return false;
        };
        // No port, no rows: a launcher that mounted the panel without providing
        // `tui-providers` gets an empty list rather than a panic — the same
        // bargain every other absent seam strikes.
        let Some(port) = ctx.service::<crate::plugin::ProvidersSvc>() else {
            return false;
        };
        // The file says which model the *next* session starts on; this one may
        // have been switched with `/model` since. The screen knows that, so it
        // marks the row rather than asking the port to know it.
        let live = self.client.described().and_then(|d| d.model);
        let view = port.rows().with_current(live.as_deref());
        self.host.show_providers(view)
    }

    /// Run one key against the providers panel, and act on what it asked for.
    ///
    /// **True when a frame is owed.**
    fn run_providers_key(&self, press: crate::surface::KeyPress) -> bool {
        let (changed, asked) = self.host.providers_key(press);
        let Some(step) = asked else {
            return changed;
        };
        match self.apply_provider_step(step) {
            Ok(Some(said)) => self.host.say(&said, false),
            Ok(None) => {}
            Err(why) => self.host.say(&why, true),
        }
        self.refresh_providers();
        true
    }

    /// Send one change over the providers seam, then tell the runtime.
    ///
    /// `Err` is the launcher's refusal, verbatim: a name the product will not
    /// take, a file it cannot write. This end does not second-guess it — the
    /// screen has no idea what the configuration accepts, which is the whole
    /// reason the port exists.
    ///
    /// **Writing the file is not making it so.** Every write here changes what
    /// the agent would be built from, so each one is followed by
    /// `HostCommand::Reload`, which reads the configuration again and rebuilds
    /// what changed — the same road the settings panel takes
    /// (`Tui::reload_if_a_change_needs_it`), and for the same reason: "the file
    /// changed but the session did not" is the state a person must not be left
    /// in silently.
    fn apply_provider_step(&self, step: crate::providers::Step) -> Result<Option<String>, String> {
        use crate::providers::Step;
        // Switching model is not a write at all: it is the gesture `/model <id>`
        // already is, dispatched as exactly that command so a panel and a typed
        // command cannot come to mean different things.
        if let Step::Use { id } = &step {
            let keys = self.wake.lock().expect("wake poisoned").clone();
            if let Some(keys) = keys {
                let _ = keys.send(Wake::Chose(Some(format!("/model {id}"))));
            }
            self.host.close_providers();
            return Ok(None);
        }
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return Err(t(Msg::ScreenNotConnectedProviders).into_owned());
        };
        let Some(port) = ctx.service::<crate::plugin::ProvidersSvc>() else {
            return Err(t(Msg::NoProviderPort).into_owned());
        };
        let said = match step {
            Step::Use { .. } | Step::Stay | Step::Close => None,
            Step::SaveAccount {
                id: Some(id),
                draft,
            } => {
                port.edit_account(&id, &draft)?;
                Some(t(Msg::ProviderEdited { id: &id }).into_owned())
            }
            Step::SaveAccount { id: None, draft } => {
                let id = port.add_account(&draft)?;
                // Straight into its model list: an account with no model under
                // it cannot be talked to, so "what now" is answered by showing
                // the one thing left to do.
                self.host.show_providers(port.rows());
                self.host.walk_into_provider(&id);
                Some(t(Msg::ProviderAddedAddModel { id: &id }).into_owned())
            }
            Step::SaveModel {
                id: Some(id),
                draft,
            } => {
                port.edit_model(&id, &draft)?;
                Some(t(Msg::ProviderEdited { id: &id }).into_owned())
            }
            Step::SaveModel { id: None, draft } => {
                let id = port.add_model(&draft)?;
                Some(t(Msg::ProviderAdded { id: &id }).into_owned())
            }
            Step::DeleteAccount { id } => {
                port.delete_account(&id)?;
                Some(t(Msg::ProviderDeletedWithModels { id: &id }).into_owned())
            }
            Step::DeleteModel { id } => {
                port.delete_model(&id)?;
                Some(t(Msg::ProviderDeleted { id: &id }).into_owned())
            }
        };
        self.reload_after_provider_change();
        Ok(said)
    }

    /// Hand the runtime a reload after a provider changed.
    ///
    /// Fire and forget, on its own task, for the reason every other host command
    /// is: a reload rebuilds the graph, and a screen that blocked on it would
    /// stop painting while it did. A failure is said out loud rather than
    /// swallowed.
    fn reload_after_provider_change(&self) {
        let Some(control) = self.client.control() else {
            return;
        };
        let root = self.client.root();
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            let outcome = control
                .call(atomcode_host_api::HostCommand::Reload { session: root })
                .await;
            if let Err(error) = outcome {
                host.say(
                    t(Msg::ConfigWrittenReloadFailed {
                        error: &format!("{error:?}"),
                    })
                    .into_owned(),
                    true,
                );
                if let Some(keys) = keys {
                    let _ = keys.send(Wake::Fact);
                }
            }
        });
    }

    /// Bring `Moment::plugins` in step with what the launcher reads.
    ///
    /// Asked when the panel opens and after every job lands, never per frame,
    /// for the reason [`Tui::refresh_providers`] is: the port reads files, and a
    /// `render` that did filesystem work would break the purity this crate rests
    /// on. **True when the rows changed.**
    fn refresh_plugins(&self) -> bool {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return false;
        };
        let Some(port) = ctx.service::<crate::plugin::PluginsSvc>() else {
            return false;
        };
        let view = port.rows();
        self.host.show_plugins(view)
    }

    /// Ask the host what the catalog looks like, and put it on screen.
    ///
    /// Out on a task rather than awaited here: it is a round trip to the tree,
    /// and the panel opens now.
    fn refresh_tools(&self) {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return;
        };
        let Some(port) = ctx.service::<crate::plugin::ToolCatalogSvc>() else {
            return;
        };
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            match port.list().await {
                Ok(view) => {
                    host.show_tools(view);
                }
                Err(why) => {
                    host.said(
                        t(Msg::ToolCatalogUnreadable { why: &why }).into_owned(),
                        true,
                    );
                }
            }
            if let Some(keys) = keys {
                let _ = keys.send(Wake::Fact);
            }
        });
    }

    /// Ask what the MCP servers are, and put them on the panel.
    ///
    /// Out on a task rather than awaited here, for the reason
    /// [`Tui::refresh_tools`] gives: it is a round trip to the running tree, and
    /// the panel opens now — an empty table for the beat it takes, not a
    /// composer that will not take a key.
    fn refresh_mcp(&self) {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            self.host
                .mcp_note(Some(t(Msg::ScreenNotConnectedAgent).into_owned()));
            return;
        };
        let Some(port) = ctx.service::<crate::plugin::McpSvc>() else {
            self.host.mcp_note(Some(t(Msg::NoMcpPort).into_owned()));
            return;
        };
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            match port.list().await {
                Ok(view) => {
                    host.show_mcp(view);
                }
                // 读不到的目录说在面板上,不静默:一张空表读起来像「一台都没有」
                // (设计 §6)。
                Err(why) => {
                    host.mcp_note(Some(why));
                }
            }
            if let Some(keys) = keys {
                let _ = keys.send(Wake::Fact);
            }
        });
    }

    /// Ask the host which turns this session can go back to, and put the answer
    /// on the panel.
    ///
    /// Read when the panel opens and again after a rewind lands, never per
    /// frame: it is a round trip to the running tree (and, for the workspace
    /// half, to a checkpoint on disk), while `render` must stay pure.
    fn refresh_rewind(&self) {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            self.host.rewind_busy(None);
            self.host
                .say(t(Msg::ScreenNotConnectedRewind).into_owned(), true);
            return;
        };
        let Some(port) = ctx.service::<crate::plugin::RewindSvc>() else {
            self.host.rewind_busy(None);
            self.host.say(t(Msg::NoRewind).into_owned(), true);
            return;
        };
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            match port.points().await {
                Ok(view) => {
                    host.show_rewind(view);
                }
                Err(why) => {
                    host.said(
                        t(Msg::RewindPointsUnreadable { why: &why }).into_owned(),
                        true,
                    );
                }
            }
            // Whatever came back, the reading is over — including the failure,
            // which must not leave a panel saying "reading…" forever.
            host.rewind_busy(None);
            if let Some(keys) = keys {
                let _ = keys.send(Wake::Fact);
            }
        });
    }

    /// Pull the rewind panel up (or put it away), and read the turns when it
    /// comes up.
    fn toggle_rewind_panel(&self) {
        if !self.host.toggle_rewind() {
            self.say(&t(Msg::NoRewindPanel));
            return;
        }
        if self.host.rewind_open() {
            self.refresh_rewind();
        }
    }

    /// Run one key against the rewind panel, and act on what it asked for.
    ///
    /// **True when a frame is owed.**
    fn run_rewind_key(&self, press: crate::surface::KeyPress) -> bool {
        let (changed, asked) = self.host.rewind_key(press);
        let Some(step) = asked else {
            return changed;
        };
        self.apply_rewind_step(step);
        true
    }

    /// One key against the resume panel. The only thing it asks for is a resume,
    /// dispatched as the command `/resume <id>` already is — so a panel and a
    /// typed command cannot come to mean different things.
    fn run_resume_key(&self, press: crate::surface::KeyPress) -> bool {
        let (changed, asked) = self.host.resume_key(press);
        if let Some(crate::resume::Step::Delete { id }) = asked {
            self.delete_session(id);
            return true;
        }
        // 走到哪一行,就去问那一行最后聊了什么。
        self.fetch_resume_preview();
        let Some(crate::resume::Step::Resume { id }) = asked else {
            return changed;
        };
        let keys = self.wake.lock().expect("wake poisoned").clone();
        if let Some(keys) = keys {
            let _ = keys.send(Wake::Chose(Some(format!("/resume {id}"))));
        }
        self.host.close_resume();
        true
    }

    /// Ask what the selected session last talked about.
    ///
    /// 每行只问一次(`resume_preview_wanted` 认这件事),答案回来时人若已经走到
    /// 别的行上就作废——按住方向键翻一遍列表不该让屏幕上闪过一串别人的对话。
    fn fetch_resume_preview(&self) {
        let Some(id) = self.host.resume_preview_wanted() else {
            return;
        };
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return;
        };
        let Some(port) = ctx.service::<crate::plugin::ResumeSvc>() else {
            return;
        };
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            let lines = port.preview(&id).await.unwrap_or_default();
            if host.show_resume_preview(&id, lines) {
                if let Some(keys) = keys {
                    let _ = keys.send(Wake::Fact);
                }
            }
        });
    }

    /// Throw a stored session away, once the panel has asked twice.
    ///
    /// The panel stays up: deleting is something a person does to a list they
    /// are still reading, unlike resuming, which is the end of the list's job.
    /// The row goes when the host says it is gone — not before, or a failed
    /// delete would leave the screen showing a session that is still there.
    fn delete_session(&self, id: String) {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            self.host
                .say(t(Msg::ScreenNotConnectedRewind).into_owned(), true);
            return;
        };
        let Some(port) = ctx.service::<crate::plugin::ResumeSvc>() else {
            self.host.say(t(Msg::NoResumeStore).into_owned(), true);
            return;
        };
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            let said = match port.delete(&id).await {
                Ok(()) => {
                    host.forget_resume(&id);
                    t(Msg::ResumeDeleted { id: &id }).into_owned()
                }
                Err(why) => t(Msg::ResumeDeleteFailed { why: &why }).into_owned(),
            };
            host.say(said, true);
            if let Some(keys) = keys {
                let _ = keys.send(Wake::Fact);
            }
        });
    }

    /// Send one rewind over the seam.
    ///
    /// **The panel goes away when it lands, and the words come back to the
    /// composer** — the same ending `/undo` has (`docs/adr/0024` §17): what the
    /// person said in the turn that was taken back is put where they type, to
    /// change and send again. A failure keeps the panel up with the reason on
    /// it, because the next thing to do is still in there.
    fn apply_rewind_step(&self, step: crate::rewind::Step) {
        use crate::rewind::Step;
        let Step::Go { turn, scope } = step else {
            return;
        };
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            self.host.rewind_busy(None);
            self.host
                .say(t(Msg::ScreenNotConnectedRewind).into_owned(), true);
            return;
        };
        let Some(port) = ctx.service::<crate::plugin::RewindSvc>() else {
            self.host.rewind_busy(None);
            self.host.say(t(Msg::NoRewind).into_owned(), true);
            return;
        };
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            let done = port.rewind(turn, scope).await;
            // The panel first: whatever happened, that trip is over.
            host.rewind_busy(None);
            match done {
                Ok(done) => {
                    host.close_rewind();
                    host.said(
                        t(Msg::RewindRestored { files: done.files }).into_owned(),
                        false,
                    );
                    if let Some(prompt) = done.prompt.filter(|p| !p.is_empty()) {
                        if let Some(keys) = keys.as_ref() {
                            // Through the loop rather than into the moment from
                            // here: putting words where a person types is an
                            // action, and actions have one implementation
                            // (`act`) whether a key, a command or this asked
                            // for them.
                            let _ = keys.send(Wake::Act(Action::Paste(prompt)));
                        }
                    }
                }
                Err(why) => {
                    host.rewind_note(Some(t(Msg::RewindFailed { why: &why }).into_owned()));
                }
            }
            if let Some(keys) = keys {
                let _ = keys.send(Wake::Fact);
            }
        });
    }

    /// Run one key against the tools panel, and act on what it asked for.
    ///
    /// **True when a frame is owed.**
    fn run_tools_key(&self, press: crate::surface::KeyPress) -> bool {
        let (changed, asked) = self.host.tools_key(press);
        let Some(step) = asked else {
            return changed;
        };
        self.apply_tools_step(step);
        true
    }

    /// Send one switch over the seam.
    ///
    /// The port answers with the catalog **after** the switch, so the panel
    /// draws what happened rather than what it asked for — a tool the config
    /// excluded, or a pattern that matched nothing, comes back unchanged and
    /// the screen says so by simply not moving.
    fn apply_tools_step(&self, step: crate::tools::Step) {
        use crate::tools::Step;
        let Step::Switch { pattern, on } = step else {
            return;
        };
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            self.host.tools_busy(None);
            self.host
                .say(t(Msg::ScreenNotConnectedTools).into_owned(), true);
            return;
        };
        let Some(port) = ctx.service::<crate::plugin::ToolCatalogSvc>() else {
            self.host.tools_busy(None);
            self.host.say(t(Msg::NoToolCatalog).into_owned(), true);
            return;
        };
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            let done = port.switch(&pattern, on).await;
            // The panel first: whatever happened, it is over.
            host.tools_busy(None);
            match done {
                Ok(view) => {
                    host.show_tools(view);
                }
                Err(why) => {
                    host.tools_note(Some(t(Msg::SwitchFailed { why: &why }).into_owned()));
                }
            }
            if let Some(keys) = keys {
                let _ = keys.send(Wake::Fact);
            }
        });
    }

    /// Run one key against the MCP panel, and act on what it asked for.
    ///
    /// **True when a frame is owed.**
    fn run_mcp_key(&self, press: crate::surface::KeyPress) -> bool {
        let (changed, asked) = self.host.mcp_key(press);
        let Some(step) = asked else {
            return changed;
        };
        self.apply_mcp_step(step);
        true
    }

    /// Send one look, or one action, over the seam.
    ///
    /// The two answers are not the same shape. A detail is one server's page and
    /// goes onto the rows the panel already has ([`Host::mcp_detail`]); an action
    /// answers with the directory **after** it happened — trust is a whole-project
    /// fact, so the list is the more useful answer either way, and the panel draws
    /// what happened rather than what was asked for.
    ///
    /// `Stay` and `Close` never arrive here: [`crate::mcp::key`] keeps those to
    /// itself, and [`Host::mcp_key`] has already acted on them.
    fn apply_mcp_step(&self, step: crate::mcp::Step) {
        use crate::mcp::Step;
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            self.host.mcp_busy(None);
            self.host
                .mcp_note(Some(t(Msg::ScreenNotConnectedAgent).into_owned()));
            return;
        };
        let Some(port) = ctx.service::<crate::plugin::McpSvc>() else {
            self.host.mcp_busy(None);
            self.host.mcp_note(Some(t(Msg::NoMcpPort).into_owned()));
            return;
        };
        // A cancel is a word to the sign-in already out there, not a trip of its
        // own: that trip answers for itself when it stops.
        if let Step::Cancel = step {
            port.cancel();
            return;
        }
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            match step {
                Step::OpenDetail { server } => match port.detail(&server).await {
                    Ok(detail) => {
                        host.mcp_detail(detail);
                    }
                    Err(why) => {
                        host.mcp_note(Some(why));
                    }
                },
                Step::Act { server, action } => {
                    let done = port.act(&server, action).await;
                    // The panel first: whatever happened, it is over.
                    host.mcp_busy(None);
                    match done {
                        Ok(view) => {
                            host.show_mcp(view);
                            // 动作已经落在这一台上了,所以页面上那份详情是旧状态;
                            // 而 `act` 答的是**目录**(不带详情),整个视图换掉之后
                            // 这一页会变成空白的「正在取详情…」。面板要是还停在这一
                            // 台,就再取一次,让人看见动作之后的样子。
                            if host.mcp_awaiting_detail(&server) {
                                match port.detail(&server).await {
                                    Ok(detail) => {
                                        host.mcp_detail(detail);
                                    }
                                    Err(why) => {
                                        host.mcp_note(Some(why));
                                    }
                                }
                            }
                        }
                        Err(why) => {
                            host.mcp_note(Some(why));
                        }
                    }
                }
                Step::Stay | Step::Close | Step::Cancel => {}
            }
            if let Some(keys) = keys {
                let _ = keys.send(Wake::Fact);
            }
        });
    }

    /// Run one key against the plugins panel, and act on what it asked for.
    ///
    /// **True when a frame is owed.**
    fn run_plugins_key(&self, press: crate::surface::KeyPress) -> bool {
        let (changed, asked) = self.host.plugins_key(press);
        let Some(step) = asked else {
            return changed;
        };
        self.apply_plugins_step(step);
        true
    }

    /// Send one plugin change over the seam.
    ///
    /// **Unlike the providers panel, this does not block on the write.** Every
    /// one of these is a `git` — a clone, a pull, a copy of a tree — and the
    /// difference between one second and ten is the network. So the panel is
    /// told a job started, the work goes out on its own task, and what comes
    /// back is said into the conversation. Until it lands the panel takes no key
    /// but Esc (`crate::plugins::key`), because every other key would start a
    /// second job while the first is still cloning.
    ///
    /// **Writing to disk is not making it so.** A plugin brings skills, commands
    /// and hooks, and none of them reach the running agent until the graph is
    /// built again — so every job that landed is followed by
    /// `HostCommand::Reload`, the same road the settings and providers panels
    /// take.
    fn apply_plugins_step(&self, step: crate::plugins::Step) {
        use crate::plugins::Step;
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            self.host
                .say(t(Msg::ScreenNotConnectedPlugins).into_owned(), true);
            return;
        };
        let Some(port) = ctx.service::<crate::plugin::PluginsSvc>() else {
            self.host.say(t(Msg::NoPluginPort).into_owned(), true);
            return;
        };
        // Giving up on a job is not a job of its own: the work is still out
        // there, and what this does is tell the port that whatever lands is no
        // longer wanted. The port is the only one that can undo it.
        if let Step::Cancel { job } = &step {
            port.cancel(job);
            self.host
                .said(t(Msg::PluginJobCancelled).into_owned(), false);
            self.refresh_plugins();
            return;
        }
        let (what, job) = match &step {
            Step::Install {
                plugin,
                marketplace,
                ..
            } => (
                t(Msg::PluginInstallingAt {
                    plugin,
                    marketplace,
                })
                .into_owned(),
                format!("{plugin}@{marketplace}"),
            ),
            Step::Update {
                plugin,
                marketplace,
                ..
            } => (
                t(Msg::PluginUpdatingAt {
                    plugin,
                    marketplace,
                })
                .into_owned(),
                format!("{plugin}@{marketplace}"),
            ),
            Step::Uninstall {
                plugin,
                marketplace,
                ..
            } => (
                t(Msg::PluginUninstallingAt {
                    plugin,
                    marketplace,
                })
                .into_owned(),
                format!("{plugin}@{marketplace}"),
            ),
            Step::AddMarket { url } => (
                t(Msg::MarketFetching { what: url }).into_owned(),
                format!("market:{url}"),
            ),
            Step::UpdateMarket { name } => (
                t(Msg::MarketUpdating { what: name }).into_owned(),
                format!("market:{name}"),
            ),
            Step::RemoveMarket { name } => (
                t(Msg::MarketRemoving { what: name }).into_owned(),
                format!("market:{name}"),
            ),
            Step::Stay | Step::Close | Step::Cancel { .. } => return,
        };
        self.host
            .plugins_busy(Some(crate::plugins::Busy { what, job }));
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        let control = self.client.control();
        let root = self.client.root();
        tokio::spawn(async move {
            let done = match step {
                Step::Install {
                    plugin,
                    marketplace,
                    scope,
                } => port.install(&plugin, &marketplace, scope).await,
                Step::Update {
                    plugin,
                    marketplace,
                    scope,
                } => port.update(&plugin, &marketplace, scope).await,
                Step::Uninstall {
                    plugin,
                    marketplace,
                    scope,
                } => port.uninstall(&plugin, &marketplace, scope).await,
                Step::AddMarket { url } => port.add_market(&url).await,
                Step::UpdateMarket { name } => port.update_market(&name).await,
                Step::RemoveMarket { name } => port.remove_market(&name).await,
                Step::Stay | Step::Close | Step::Cancel { .. } => return,
            };
            // The panel first: whatever happened, it is over, and a panel still
            // saying "installing…" after the line that says it failed is a panel
            // that has to be closed to be believed.
            host.plugins_busy(None);
            host.show_plugins(port.rows());
            match done {
                Ok(said) => {
                    host.said(said, false);
                    // What is on disk is not what the agent is running until the
                    // graph is built again.
                    if let Some(control) = control {
                        if let Err(error) = control
                            .call(atomcode_host_api::HostCommand::Reload { session: root })
                            .await
                        {
                            // Not "installed, but…": the line above already
                            // said what happened, and this one runs after an
                            // uninstall and a marketplace change too.
                            host.said(
                                t(Msg::ReloadFailedAfterPlugin {
                                    why: &crate::commands::refusal(error),
                                })
                                .into_owned(),
                                true,
                            );
                        }
                    }
                }
                Err(why) => host.said(why, true),
            }
            if let Some(keys) = keys {
                let _ = keys.send(Wake::Fact);
            }
        });
    }

    /// Ask the host what the Status page shows, and repaint when it answers.
    ///
    /// Five questions, asked at once rather than one after another: they are
    /// five round trips about one screen, and a page that took five times as
    /// long to appear is a page that feels broken. Each stands on its own —
    /// a host that will not say who is signed in is not a reason to leave the
    /// working directory blank.
    fn fetch_status(&self) {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return;
        };
        let Some(control) = self.client.control() else {
            return;
        };
        let session = self.client.session();
        let described = self.client.described();
        let host = self.host.clone();
        let repaint = ctx.service::<RepaintSvc>();
        tokio::spawn(async move {
            let ask = |command| {
                let control = control.clone();
                async move { control.call(command).await.ok() }
            };
            let (context, who, mcp, usage, sources) = tokio::join!(
                ask(HostCommand::Context {
                    session: session.clone()
                }),
                ask(HostCommand::WhoAmI {
                    session: session.clone()
                }),
                ask(HostCommand::McpStatus {
                    session: session.clone()
                }),
                ask(HostCommand::Usage {
                    session: session.clone(),
                    windows_only: false,
                }),
                ask(HostCommand::Sources {
                    session: session.clone()
                }),
            );
            let cwd = match context {
                Some(HostReply::Context { working_dir, .. }) => working_dir,
                _ => String::new(),
            };
            let who = match who {
                Some(HostReply::Identity {
                    signed_in: true,
                    who: Some(who),
                    detail,
                }) => Some((who, detail)),
                _ => None,
            };
            let mcp = match mcp {
                Some(HostReply::McpServers { servers }) => servers,
                _ => Vec::new(),
            };
            let (plan, window) = match usage {
                Some(HostReply::Usage { plan, windows, .. }) => (plan, windows.into_iter().next()),
                _ => (None, None),
            };
            let sources = match sources {
                Some(HostReply::Sources { groups }) => groups,
                _ => Vec::new(),
            };
            {
                let mut moment = host.moment.write().expect("moment poisoned");
                let title = moment.title.clone();
                moment.status = Some(crate::settings::StatusPage {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    session,
                    title,
                    cwd,
                    who,
                    model: described.as_ref().and_then(|d| d.model.clone()),
                    effort: described
                        .as_ref()
                        .and_then(|d| d.reasoning_effort)
                        .map(|level| level.as_str().to_string()),
                    plan,
                    window,
                    mcp,
                    sources,
                });
            }
            if let Some(repaint) = repaint {
                repaint.now();
            }
        });
    }

    /// Ask how much allowance is left, at most this often.
    ///
    /// Tied to a turn ending rather than to a timer, and then held to this
    /// interval: a turn is when the figure can have moved, and the question
    /// costs a round trip on the account. Idle, nothing is asked at all.
    fn check_allowance(&self) {
        let now = std::time::Instant::now();
        {
            let mut last = self.allowance_checked.lock().expect("allowance poisoned");
            if last.is_some_and(|at| now.duration_since(at) < ALLOWANCE_EVERY) {
                return;
            }
            // Stamped before the answer comes back, not after: two turns
            // finishing inside the interval must not both send a question
            // while the first is still in flight.
            *last = Some(now);
        }
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return;
        };
        let Some(control) = self.client.control() else {
            return;
        };
        let session = self.client.session();
        let host = self.host.clone();
        let repaint = ctx.service::<RepaintSvc>();
        tokio::spawn(async move {
            // The cheap form — the windows and nothing else. The plan behind
            // them and what has been spent are two more round trips, and
            // neither is on this row.
            let windows = match control
                .call(HostCommand::Usage {
                    session,
                    windows_only: true,
                })
                .await
            {
                Ok(HostReply::Usage { windows, .. }) => windows,
                // A host that will not say leaves the row as it was. Blanking
                // it on a failed question would read as "you are back to zero".
                _ => return,
            };
            let nearest = crate::moment::Allowance::nearest(&windows);
            let changed = {
                let mut m = host.moment.write().expect("moment poisoned");
                let changed = m.allowance != nearest;
                m.allowance = nearest;
                changed
            };
            if changed {
                if let Some(repaint) = repaint {
                    repaint.now();
                }
            }
        });
    }

    /// Ask the host what the Usage page shows, and repaint when it answers.
    ///
    /// Asked rather than waited for: an allowance window moves on the server's
    /// clock, so there is nothing to subscribe to — it is fetched when the page
    /// is opened. On its own task because both questions are round trips and a
    /// screen that blocked on them would stop painting; the repaint seam is how
    /// the answer gets drawn, since nothing about it came from a keystroke.
    fn fetch_usage(&self) {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return;
        };
        let Some(control) = self.client.control() else {
            return;
        };
        let session = self.client.session();
        let host = self.host.clone();
        let repaint = ctx.service::<RepaintSvc>();
        tokio::spawn(async move {
            let context = match control
                .call(HostCommand::Context {
                    session: session.clone(),
                })
                .await
            {
                Ok(HostReply::Context {
                    window,
                    used,
                    model,
                    ..
                }) if window > 0 => Some(crate::settings::ContextUse {
                    model,
                    used,
                    window,
                }),
                _ => None,
            };
            let (windows, plan, stats) = match control
                .call(HostCommand::Usage {
                    session,
                    windows_only: false,
                })
                .await
            {
                Ok(HostReply::Usage {
                    windows,
                    plan,
                    stats,
                }) => (windows, plan, stats),
                // A host that will not say is a host with nothing to draw; the
                // page says so rather than showing an error where a chart goes.
                _ => (Vec::new(), None, None),
            };
            host.moment.write().expect("moment poisoned").usage =
                Some(crate::settings::UsagePage {
                    context,
                    windows,
                    plan,
                    stats,
                });
            if let Some(repaint) = repaint {
                repaint.now();
            }
        });
    }

    /// Run one key against the settings panel, and act on what it asked for.
    ///
    /// One implementation for two callers, which is the point: the keyboard path
    /// and a click both end up here. A pointer press that reached a setting has
    /// the row armed and then runs *this* with a return, rather than a second
    /// copy of "confirm the pointed row" that would agree with the keyboard
    /// until one of them was changed.
    ///
    /// **True when a frame is owed.**
    fn run_settings_key(&self, press: crate::surface::KeyPress) -> bool {
        let (changed, asked) = self.host.settings_key(press);
        if let Some(step) = asked {
            let done = match &step {
                crate::settings::Step::Set { id, value } => self.apply_setting(id, value),
                // Unsetting goes over the same seam and through the same
                // reload: what the running graph has to be told is that the
                // configuration changed, not which way.
                crate::settings::Step::Reset { id } => self.reset_setting(id),
                _ => Ok(()),
            };
            match done {
                Ok(()) => {
                    self.refresh_settings();
                }
                Err(why) => self.host.say(&why, true),
            }
            return true;
        }
        changed
    }

    /// Send one change over the settings seam, then tell the runtime about it.
    ///
    /// `Err` is the launcher's refusal, verbatim: a value the product will not
    /// take, or a file it cannot write. This end does not second-guess it — the
    /// screen has no idea what the configuration accepts, which is the whole
    /// reason the port exists.
    ///
    /// **Writing the file is not making it so.** A setting that says `Reload` or
    /// `Reprepare` is one the running graph has to be told about, and the only
    /// way to tell it is `HostCommand::Reload` — which reads the configuration
    /// again and rebuilds what changed (`atomcode-coding/src/front_end.rs`).
    /// The distinction is the row's `applies`, not a list of ids here: a setting
    /// whose policy is `Restart` is one a reload would not reach anyway, so
    /// asking is skipped rather than answered with a lie.
    fn apply_setting(&self, id: &str, value: &str) -> Result<(), String> {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return Err(t(Msg::ScreenNotConnectedSettings).into_owned());
        };
        let Some(port) = ctx.service::<crate::plugin::SettingsSvc>() else {
            return Err(t(Msg::NoSettingsPort).into_owned());
        };
        port.set(id, value)?;
        self.reload_if_a_change_needs_it(id)
    }

    /// The same, for putting one back to the build's own answer.
    ///
    /// Everything after the write is identical, which is why it is the same
    /// call: what the running graph has to be told is that the configuration
    /// changed, and it reads the file again either way.
    fn reset_setting(&self, id: &str) -> Result<(), String> {
        let Some(ctx) = self.ctx.lock().expect("ctx poisoned").clone() else {
            return Err(t(Msg::ScreenNotConnectedSettings).into_owned());
        };
        let Some(port) = ctx.service::<crate::plugin::SettingsSvc>() else {
            return Err(t(Msg::NoSettingsPort).into_owned());
        };
        port.reset(id)?;
        self.reload_if_a_change_needs_it(id)
    }

    /// Hand the runtime a reload, when the setting that changed is one it can
    /// act on.
    ///
    /// Fire and forget, on its own task, for the reason every other host command
    /// is: a reload rebuilds the graph and a screen that blocked on it would
    /// stop painting while it did. A failure is said out loud rather than
    /// swallowed — "the file changed but the session did not" is exactly the
    /// state a person must not be left in silently.
    fn reload_if_a_change_needs_it(&self, id: &str) -> Result<(), String> {
        let needs = {
            let m = self.host.moment.read().expect("moment poisoned");
            m.settings
                .rows()
                .iter()
                .find(|row| row.id == id)
                .map(|row| {
                    matches!(
                        row.applies,
                        crate::settings::Applies::Reload | crate::settings::Applies::Reprepare
                    )
                })
                .unwrap_or(false)
        };
        if !needs {
            return Ok(());
        }
        let Some(control) = self.client.control() else {
            return Ok(());
        };
        let root = self.client.root();
        // The host itself, not a wake: the loop owns the wake channel, and a
        // command's failure has to reach the tip row from a task that is not the
        // loop. `Host::say` is the one way to say something on the screen, and
        // it wakes the loop for the frame it lands in.
        let host = self.host.clone();
        let keys = self.wake.lock().expect("wake poisoned").clone();
        tokio::spawn(async move {
            let outcome = control
                .call(atomcode_host_api::HostCommand::Reload { session: root })
                .await;
            if let Err(error) = outcome {
                // Rendered by the same function the commands use, so one host
                // error reads the same wherever it surfaces.
                host.say(
                    t(Msg::SettingWrittenReloadFailed {
                        why: &crate::commands::refusal(error),
                    })
                    .into_owned(),
                    true,
                );
                if let Some(keys) = keys {
                    let _ = keys.send(Wake::Fact);
                }
            }
        });
        Ok(())
    }

    fn refresh_members(&self) {
        let members = self.members.moment_members();
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
        // `switch_view` left the line at Idle; what the session last said puts
        // it back. The lead too, not only the members the roster knows: its
        // turn events are dropped while a member is on screen, so coming back
        // to a lead that is still working found it "idle" — and esc, reading
        // that, armed a double-tap instead of stopping the turn.
        let activity = match self.client.status_of(session) {
            Some(AgentStatus::Working) => Some(crate::moment::Activity::Working),
            Some(AgentStatus::Stopping) => Some(crate::moment::Activity::Stopping),
            _ if self.members.is_working(session) => Some(crate::moment::Activity::Working),
            _ => None,
        };
        let mut m = self.host.moment.write().expect("moment poisoned");
        m.viewing = session.to_string();
        if let Some(activity) = activity {
            m.activity = activity;
        }
        true
    }

    /// Tell the screen which turns were taken back, from the facts it has —
    /// and which of them a *rewind* took, which is drawn differently.
    fn mark_undone(&self) -> bool {
        let events = self.client.events();
        self.host.mark_undone(
            atomcode_kernel::session::undone_turns(&events),
            atomcode_kernel::session::rewound_turns(&events),
        )
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
                if let Some(session) = self.host.team_target() {
                    self.switch_to(&session);
                }
                self.host.unfocus_team();
            }
            (Key::Esc, _) | (Key::Tab, _) | (Key::BackTab, _) => {
                self.host.unfocus_team();
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
                t(Msg::TeamLead).into_owned()
            } else {
                session.rsplit('/').next().unwrap_or(session).to_string()
            };
            self.host
                .say(t(Msg::NowViewing { name: &name }).into_owned(), false);
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
            self.say(&t(Msg::PolicyBlockedNoWayOut));
            return;
        }
        let question = atomcode_harness::seams::Question {
            prompt: t(Msg::PolicyQuestion).into_owned(),
            options,
            asker: Some(t(Msg::PolicyAsker).into_owned()),
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
                // The turn's opening line is on screen now (its own message,
                // normally; an assistant's first word for a turn nobody typed).
                // Let the working status catch up — it was armed at `TurnStarted`
                // and held back so it never draws above this line. Structural
                // facts (`TurnStart`/`TurnEnd`/`Titled`) draw nothing, so they do
                // not spend the arm — otherwise the spinner would settle on the
                // `TurnStart` that the log writes *before* the message.
                if matches!(
                    committed.event,
                    atomcode_kernel::session::SessionEvent::UserMessage { .. }
                        | atomcode_kernel::session::SessionEvent::AssistantMessage { .. }
                ) && !self.host.settle_working()
                {
                    // Nothing was armed — the turn's start has not reached
                    // this screen yet. The facts come by the feed and the
                    // start by the runtime, two roads with no order between
                    // them, and when the fact wins, the arm that follows
                    // waits for a fact that has already gone by: the line
                    // stayed down for the whole turn. It cost a person five
                    // minutes of blank screen against a model that had
                    // opened its stream and gone quiet.
                    //
                    // The agent's own status is the other half, and it comes
                    // by the same road as the facts, ahead of them (it is set
                    // when the turn opens) — so it is here, and it is true.
                    // Raised only from idle, and only on a fact, so the row
                    // still lands under the message the arm existed to
                    // protect, and a stop in progress is not written over.
                    let idle = self.host.moment.read().expect("moment poisoned").activity
                        == Activity::Idle;
                    if idle
                        && matches!(
                            self.client.status_of(&self.client.session()),
                            Some(AgentStatus::Working)
                        )
                    {
                        self.set_activity(Activity::Working);
                    }
                }
                true
            }
            AgentEvent::Described { description } => {
                self.client.describe(&description);
                // Keep the status row's context window in step with the screen
                // agent's mounted model. Read it back off the screen agent's
                // description (not the one that just arrived, which may be a team
                // member's), so switching models moves the denominator the row
                // shows the used tokens against. A change is stale — the footer
                // must repaint to show the new window.
                let described = self.client.described();
                let window = described
                    .as_ref()
                    .and_then(|d| d.context_window)
                    .unwrap_or(0);
                let model = described
                    .as_ref()
                    .and_then(|d| d.model.clone())
                    .unwrap_or_default();
                // The thinking level, on the same terms as the window and the
                // model: it is the agent's, and the row draws it beside the
                // model the way tuix does (`glm-5 [high]`).
                let effort = described.as_ref().and_then(|d| d.reasoning_effort);
                let mut moment = self.host.moment.write().expect("moment poisoned");
                let changed =
                    moment.ctx_window != window || moment.model != model || moment.effort != effort;
                moment.ctx_window = window;
                moment.model = model;
                moment.effort = effort;
                changed
            }
            AgentEvent::Accepted { command, .. } => {
                self.client.answered(&command);
                false
            }
            AgentEvent::Rejected { command, error } => {
                self.client.answered(&command);
                self.say_refused(&t(Msg::NotDelivered {
                    error: &format!("{error:?}"),
                }));
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
                let mut changed = self.members.set_status(&session, status);
                // A member on screen has no turn events on this connection —
                // those are the root's — so its status is what moves the line.
                if on_screen && session != self.client.root() {
                    use crate::moment::Activity;
                    changed |= self.set_activity(match status {
                        AgentStatus::Idle => Activity::Idle,
                        AgentStatus::Working => Activity::Working,
                        AgentStatus::Stopping => Activity::Stopping,
                    });
                } else if on_screen && status == AgentStatus::Idle && self.client.settled() {
                    // The lead's own line is moved by its turn events, which say
                    // more than a status does — but only while they arrive. When
                    // one does not, the claim on screen (正在停止, most of all)
                    // has nothing to take it back, and it stood until the next
                    // turn. The agent saying it is idle, with nothing sent to it
                    // still unclaimed, is the backstop: the screen never keeps
                    // insisting on work the agent says is over.
                    changed |= self.set_activity(crate::moment::Activity::Idle);
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
                self.members.note(
                    &description.session,
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
                // Not on the panel any more — a stopped member is not one of
                // the rows — but its log is kept for `/agents`, which lists
                // every member and switches to any of them (`docs/adr/0023` §5).
                let kept = self.members.mark_gone(&session);
                // And if the screen was on it, the lead takes the screen back.
                // The panel has no row for a stopped member to switch back
                // from, so without this the person would be left reading a
                // conversation with nothing on screen offering a way out.
                if self.client.session() == session && session != self.client.root() {
                    let root = self.client.root();
                    self.switch_to(&root);
                }
                kept
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
            // Arm the working line rather than raise it here: it settles when the
            // turn's first fact is folded (`AgentEvent::Fact`), so "正在等待模型"
            // never paints a frame ahead of the message that started the turn.
            AgentEvent::TurnStarted { .. } => self.host.arm_working(),
            // A cancel (whoever asked for it) or a failure can end the turn with
            // words still in the inbox — nothing folded them, and no `Steered` is
            // coming. The panel is a claim about the model's inbox, so it goes
            // with the turn rather than lying about work that will not happen. The
            // `已中断` note is not raised here: a self-cancel already marked it on
            // the key (`Action::Escape`), and an internal cancel must not.
            AgentEvent::TurnComplete { .. } | AgentEvent::Cancelled => {
                self.host.clear_steering();
                // A turn just spent some of the allowance, so this is the
                // moment the figure changed. Rate-limited inside.
                self.check_allowance();
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
                    self.say(&t(Msg::Compacted));
                } else {
                    self.say(&t(Msg::NothingWorthCompacting));
                }
                true
            }
            // The other half of `Compacted`, and it used to fall through to
            // `_ => false`: a `/compact` that could not write its checkpoint
            // said nothing at all, so what a person saw was a command that did
            // not answer. Nothing else reports it — there is no notice kind for
            // it, and the runtime's own event is this one.
            AgentEvent::CompactionFailed { error, .. } => {
                self.set_activity(Activity::Idle);
                self.say_refused(&t(Msg::CompactFailed {
                    error: &error.to_string(),
                }));
                true
            }
            AgentEvent::Error { message, .. } => {
                // Said always; the line below it only when the agent is not
                // working. Not every error ends a turn — a `/cancel-all` refused
                // by an idle member, a mid-turn cost warning, a persistence
                // warning — and writing "idle" over a turn that is still running
                // takes the spinner (and the 停止中) off a screen with work on
                // it. The claim is only ever lowered here, never raised: raising
                // it belongs to the turn's own first fact.
                let working = matches!(
                    self.client.status_of(&self.client.session()),
                    Some(AgentStatus::Working) | Some(AgentStatus::Stopping)
                );
                if !working {
                    self.set_activity(Activity::Idle);
                }
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

    /// Ask the agent on screen to stop, and say so only if it has something to
    /// stop.
    ///
    /// [`Activity::Stopping`] is a claim about *now*, and it has exactly one
    /// thing that can take it back: a turn ending, which arrives as
    /// `TurnComplete`/`Cancelled` — and those are only sent when a turn was
    /// running. Asking an idle agent to cancel is deliberately a no-op (the
    /// harness says so where it defines `cancel`), so a front end that writes
    /// `Stopping` first leaves the claim standing with no fact coming.
    ///
    /// What that looked like: pressing esc on an idle screen raised a live line
    /// reading 正在停止 and a status line reading 停止中, both of which stayed up
    /// until the next turn. Worse, that state is what a submission is read
    /// against — "a turn is running, so this line is steering" — so the next
    /// thing typed appeared in the steering panel as work the model was about to
    /// be handed, when in fact it opened a fresh turn ([`crate::modules::steering`]).
    ///
    /// So the command is always sent (a cancel is never wrong to ask for; it is
    /// how a turn that just opened and has not yet marked itself working is
    /// still stoppable — including the arm window, where the turn is in flight
    /// but `activity` still reads `Idle` until its first fact), and the *words*
    /// are said only while the agent is visibly working. Both routes that move
    /// `activity` to `Working` — the status stream and `Host::settle_working` —
    /// only fire for a real in-flight turn, so `Working` here means one is.
    ///
    /// [`Activity::Stopping`]: crate::moment::Activity::Stopping
    fn stop_turn(&self, client: &AgentClient) {
        client.cancel();
        let working = self.host.moment.read().expect("moment poisoned").activity
            == crate::moment::Activity::Working;
        if working {
            self.host.set_activity(crate::moment::Activity::Stopping);
        }
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

    /// Open attached image `n` in the person's desktop viewer, the way clicking a
    /// file does. `image` is its bytes when the caller still had them in hand
    /// (a composer click); `None` means look them up in the session gallery (a
    /// click on a sent line in the history). The bytes are written to a temp file
    /// once — reused on later clicks — and opened on this machine's desktop.
    ///
    /// The opener is [`LocalOpener`] directly, **not** the `OpenerSvc` seam: that
    /// service is mounted in the agent runtime's plexus context, and this front
    /// end has its own — requiring it here fails ("no opener"). Direct is also the
    /// honest answer, because a person clicking in a full-screen terminal *is*
    /// sitting at the machine the picture should open on.
    ///
    /// Best-effort by design: the failures a person can do anything about (no
    /// bytes, cannot write the file) are said out loud; the open itself runs
    /// off-thread, since `act` is not async and a desktop launcher must not block
    /// the render loop.
    fn preview_image(&self, n: usize, image: Option<atomcode_kernel::message::ImageContent>) {
        let path = match self.image_temp_file(n, image) {
            Ok(path) => path,
            Err(reason) => {
                self.say_refused(&t(Msg::ImagePreviewFailed { reason: &reason }));
                return;
            }
        };
        // Off the render loop: launching Preview.app (`open`, `xdg-open`, …) is a
        // process spawn, and `act` returns to paint. A failure here is rare and
        // not actionable, so it is left to the opener's own logging.
        tokio::spawn(async move {
            use atomcode_capabilities::tools::Opener as _;
            let _ = atomcode_capabilities::tools::LocalOpener
                .open(&atomcode_capabilities::tools::OpenTarget::Path(path))
                .await;
        });
    }

    /// The on-disk path of attached image `n`, written once and cached. `image`
    /// is the bytes if the caller had them; otherwise they come from the gallery.
    /// The extension follows the media type so the desktop viewer opens it right.
    fn image_temp_file(
        &self,
        n: usize,
        image: Option<atomcode_kernel::message::ImageContent>,
    ) -> Result<std::path::PathBuf, String> {
        use base64::Engine as _;

        let mut cache = self.image_files.lock().expect("image files poisoned");
        if let Some(path) = cache.get(&n) {
            if path.exists() {
                return Ok(path.clone());
            }
        }
        let image = image.ok_or_else(|| t(Msg::ImageGone).into_owned())?;
        let ext = match image.media_type.as_str() {
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "png",
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(image.data.as_bytes())
            .map_err(|_| t(Msg::ImageCorrupt).into_owned())?;
        // A per-run subdirectory: `std::process::id` scopes it so two sessions do
        // not clobber each other's `[Image #1]`, and on unix it is created private
        // (0700) so another local user on a shared machine cannot read a person's
        // screenshots or pre-create a file for the viewer to open — the temp dir
        // is world-traversable and the path would otherwise be predictable.
        let dir = std::env::temp_dir().join(format!("atomcode-images-{}", std::process::id()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("img-{n}.{ext}"));
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        cache.insert(n, path.clone());
        Ok(path)
    }

    /// Whether this press steps the execution mode on.
    ///
    /// The rule, ported from the reference front end so neither loses the
    /// gesture: `BackTab` (how most terminals encode Shift+Tab) and `Tab+SHIFT`
    /// (how a few do) always cycle; plain `Tab` cycles too when
    /// `ui.mode_switch_key = "tab"`, the phone's setting, where Shift+Tab
    /// cannot be sent at all. Ctrl/Alt/Cmd+Tab never cycle — those are the
    /// terminal's own chords, and a UI that swallowed them would be eating keys
    /// that were never meant for it.
    ///
    /// Plain Tab additionally yields to the completion menu and the team panel,
    /// which is the caller's business, not this predicate's: both of those are
    /// about a list that is up, and this only answers "is this the mode key".
    fn is_mode_cycle_key(&self, press: crate::surface::KeyPress) -> bool {
        use crate::surface::Key;
        // Only bare or Shift-modified chords qualify.
        if press.mods.ctrl || press.mods.alt || press.mods.cmd {
            return false;
        }
        if press.key == Key::BackTab || (press.key == Key::Tab && press.mods.shift) {
            return true;
        }
        press.key == Key::Tab && !press.mods.shift && self.host.mode_switch_on_tab()
    }

    /// Step the execution mode on to the next one, the way Shift+Tab does.
    ///
    /// The mode is asked of the screen's own state rather than tracked a second
    /// time: what the host last pushed is the truth, and a counter here would be
    /// a second answer that could disagree after a `/mode` typed at another
    /// front end.
    ///
    /// The change goes out as the command a person would type, so the key, the
    /// word and `/mode` are one implementation. `deliver` reads the command's
    /// own line back, which is also how the person is told what happened — the
    /// same sentence `/mode` prints.
    fn cycle_mode(&self) -> bool {
        // A mode the host has not reported cannot be stepped from: the next one
        // is the *following* mode, and guessing `ask` as the starting point would
        // cycle into `accept edits` on a screen whose session might be `auto`.
        // Nothing happens instead, which is the honest answer — and the update
        // that carries the mode is already on its way (`HostCommand::Mode` asked
        // at startup, `ModeChanged` after).
        let Some(mode) = self.host.moment.read().expect("moment poisoned").mode else {
            return false;
        };
        let line = format!("/mode {}", crate::commands::mode_word(mode.next()));
        self.run_command(&line);
        false
    }

    /// Apply one action. Returns `true` to quit.
    fn act(&self, action: Action, client: &AgentClient) -> bool {
        let mut m = self.host.moment.write().expect("moment poisoned");
        // Any action other than a repeat Ctrl+C disarms the "press again to quit"
        // latch, and takes the exit hint below the box down with it: an accidental
        // first press followed by real work must never leave the terminal one
        // keystroke from exit, nor the hint up while that work goes on. First,
        // before any early return below (e.g. Escape clearing a selection) can
        // skip it.
        if !matches!(action, Action::Cancel) {
            m.disarm_quit();
        }
        // The same shape, for the other latch: every action but an Escape drops
        // the pending double-tap. Someone who pressed Esc once and went back to
        // work must not find a panel under their next stray press — the gesture
        // says nothing on screen, so it has to be the quieter of the two.
        if !matches!(action, Action::Escape) {
            m.disarm_escape();
        }
        // A highlight is a rectangle of screen cells. Anything that repaints
        // those cells with different text leaves it pointing at the wrong
        // words, so it is dropped by everything except the gestures that are
        // *about* it.
        if !matches!(
            action,
            Action::SelectFrom(..)
                | Action::SelectTo(..)
                | Action::SelectWord(..)
                | Action::SelectLine(..)
                | Action::CopySelection
                | Action::ClearSelection
        ) {
            // Escape does the innermost thing, and the selection is the
            // innermost of them.
            if m.selection.take().is_some() && matches!(action, Action::Escape) {
                // It counts as the first tap all the same: clearing a highlight
                // is what *this* press did, and the next one is still the second
                // of a double-tap.
                m.arm_escape();
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
                // The composer shows `[Pasted #N …]` markers to stay terse; the
                // model gets the whole paste — put the bodies back before
                // anything reads the text.
                let text = crate::moment::expand_pastes(&m.input, &m.pastes);
                let text = text.trim().to_string();
                // Take the pictures the text still shows before the text is
                // cleared: what was written and what was attached have to be
                // decided together, or an attachment can outlive the marker
                // that was the only reason it was going.
                let images = m.attachments.take_shown(&text);
                m.input.clear();
                m.clear_pastes();
                m.caret = 0;
                m.history_at = None;
                m.draft.clear();
                m.scroll = crate::moment::ScrollPos::BOTTOM;
                drop(m);
                self.refresh_menu();
                if text.is_empty() {
                    return false;
                }
                // A slash *command* goes to the command surface, everything else
                // to the model. The one place the two are told apart — and a
                // filesystem path that merely begins with `/` (`/Users/me/x.png`)
                // is NOT a command: it reaches the model untouched instead of
                // erroring with "没有 /Users/… 这条命令".
                if crate::command::looks_like_command(&text) {
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
                let steering = {
                    let mut m = self.host.moment.write().expect("moment poisoned");
                    // Kept for an Escape that stops the next running turn to hand
                    // back (see the `Escape` arm); the `已中断` note is stale the
                    // instant a new prompt is on its way.
                    m.last_sent = Some(text.clone());
                    m.interrupted = false;
                    m.turn_in_flight()
                };
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
                    // A whole `[Image #N]` deletes as one chip: the caret sitting
                    // just past it (`caret == end`) — or, defensively, inside it —
                    // removes the marker, and since the text no longer mentions it,
                    // its attachment goes too (checked at send by `take_shown`).
                    if let Some(span) = crate::attach::marker_spans(&m.input)
                        .into_iter()
                        .find(|s| s.start < m.caret && m.caret <= s.end)
                    {
                        m.input.replace_range(span.clone(), "");
                        m.caret = span.start;
                    } else {
                        let mut at = m.caret - 1;
                        while at > 0 && !m.input.is_char_boundary(at) {
                            at -= 1;
                        }
                        m.input.remove(at);
                        m.caret = at;
                    }
                }
            }
            Action::DeleteForward => {
                // The character the caret is ON, which is the one after it in
                // byte order. Walk to the next boundary rather than assuming
                // one byte: a `中` is three.
                let at = m.caret.min(m.input.len());
                if at < m.input.len() {
                    let mut end = at + 1;
                    while end < m.input.len() && !m.input.is_char_boundary(end) {
                        end += 1;
                    }
                    m.input.replace_range(at..end, "");
                    m.caret = at;
                }
            }
            Action::DeleteToEnd => {
                let at = m.caret.min(m.input.len());
                m.input.truncate(at);
                m.caret = at;
            }
            Action::CycleModel { forward } => {
                drop(m);
                // Read the list fresh: it carries which model is in use, and a
                // `/model` since the last read would leave a stale answer
                // stepping from the wrong place.
                self.refresh_providers();
                let next = {
                    let m = self.host.moment.read().expect("moment poisoned");
                    m.providers.model_after(forward).map(|row| row.id.clone())
                };
                // Through `/model`, which is how the panel does it too: one
                // road into a model change, and it already says what it landed
                // on. Nothing configured to step to is not an error — it is a
                // key that has nothing to do, which `/model` says better than
                // a silent no-op would.
                if let Some(id) = next {
                    self.run_command(&format!("/model {id}"));
                }
                return false;
            }
            Action::DeleteWord => {
                // A marker is one word: Ctrl+W with the caret just past (or inside)
                // an `[Image #N]` removes the whole chip, never the `7]` tail that
                // the space inside the marker would otherwise cut back to.
                if let Some(span) = crate::attach::marker_spans(&m.input)
                    .into_iter()
                    .find(|s| s.start < m.caret && m.caret <= s.end)
                {
                    m.input.replace_range(span.clone(), "");
                    m.caret = span.start;
                } else {
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
            }
            Action::Clear => {
                m.input.clear();
                // The `[Pasted …]` markers went with the text, so the bodies they
                // stood for go too.
                m.clear_pastes();
                // The markers went with the text, so the images they stood for
                // go too. Leaving them held would make the composer's state
                // disagree with the only place a person can see it.
                m.attachments.clear();
                m.caret = 0;
            }
            Action::CaretLeft => {
                // Step over an `[Image #N]` as one chip rather than into it — the
                // caret must never land between a marker's characters.
                if let Some(span) = crate::attach::marker_spans(&m.input)
                    .into_iter()
                    .find(|s| s.start < m.caret && m.caret <= s.end)
                {
                    m.caret = span.start;
                } else {
                    let mut at = m.caret.saturating_sub(1);
                    while at > 0 && !m.input.is_char_boundary(at) {
                        at -= 1;
                    }
                    m.caret = at;
                }
            }
            Action::CaretRight => {
                // At the end of the line, right takes the ghost — the shell
                // gesture. Anywhere else it is still just a caret move, and
                // with no ghost it does nothing, so the key never surprises.
                if accept_ghost(&mut m) {
                    return false;
                }
                // Step over an `[Image #N]` as one chip.
                if let Some(span) = crate::attach::marker_spans(&m.input)
                    .into_iter()
                    .find(|s| s.start <= m.caret && m.caret < s.end)
                {
                    m.caret = span.end;
                } else {
                    let mut at = (m.caret + 1).min(m.input.len());
                    while at < m.input.len() && !m.input.is_char_boundary(at) {
                        at += 1;
                    }
                    m.caret = at;
                }
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
                    let landed = input::offset_at(&m.input, to, col, w);
                    // Vertical movement maps by column and can land between a
                    // marker's characters; snap it out so the chip invariant holds
                    // for Up/Down too, not only the horizontal arrows.
                    m.caret = crate::attach::snap_caret_out_of_marker(&m.input, landed);
                } else if up {
                    recall_back(&mut m);
                } else {
                    recall_forward(&mut m);
                }
                return false;
            }
            Action::Paste(text) => {
                // Release the moment lock before probing the paste as a file path:
                // `image_from_path` may read and decode a file up to
                // `MAX_PATH_IMAGE_BYTES`, and holding the write lock across that
                // would freeze every reader of the moment for the whole read.
                drop(m);
                // A pasted image-*file* path is an attachment intent, not prose:
                // WeChat/iTerm2 save the clipboard image to a temp file and paste
                // its path, and Finder drag/copy types the path (or a `file://`
                // URL). Load the bytes now and drop in an `[Image #N]` marker
                // instead of leaving the raw path in the composer (the reported
                // "全部展示成路径"). And Cmd+V — swallowed by the terminal, so it
                // never reaches the `AttachImage` key Ctrl+V is bound to — arrives
                // here as a paste whose text is not a path; `image_for_paste` then
                // recovers the picture straight from the clipboard, so Cmd+V
                // attaches a screenshot the same as Ctrl+V. Reading at paste time
                // keeps the attachment self-contained. Anything that is neither a
                // path nor a clipboard image falls through to the text path.
                if let Some(image) = crate::attach::image_for_paste(&text, self.surface.as_ref()) {
                    // Only a conversation with no model at all has nowhere to put a
                    // picture; a text-only model has the runtime caption it (a
                    // configured or auto-detected VL helper) or say so on send.
                    if let Err(reason) = images_reach_the_model(client) {
                        self.say_refused(&reason);
                        return false;
                    }
                    self.host
                        .moment
                        .write()
                        .expect("moment poisoned")
                        .insert_image(image);
                } else {
                    // A big block folds into a `[Pasted #N …]` marker rather than
                    // filling the composer; the body is put back at submit
                    // (`expand_pastes`). Paste the same block again to expand it.
                    let clean = sanitize_paste(&text);
                    let mut m = self.host.moment.write().expect("moment poisoned");
                    let now = m.now;
                    m.insert_paste(&clean, now);
                }
                // The line changed, so the slash menu may need to change with it —
                // the same refresh the fall-through path gives every line edit.
                self.refresh_menu();
                return false;
            }
            Action::AttachImage => {
                // Refused only when there is no model at all; a text-only model
                // has the runtime caption the image (a configured or auto-detected
                // VL helper) or report on send that it could not.
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
                    self.say_refused(&t(Msg::ClipboardHasNoImage));
                    return false;
                };
                m.insert_image(image);
            }
            Action::Cancel => {
                // A turn in flight: Ctrl+C stops it and clears any pending quit —
                // the same live-line pin a turn's start gets (this is the third
                // route that moves that row). Stopping counts as in-flight too. The
                // line is left alone: cancelling the model's answer is not the same
                // gesture as clearing what you were about to say next. In flight
                // by the agent's account too, as for `Action::Escape`.
                if m.turn_in_flight() || !client.settled() {
                    m.disarm_quit();
                    drop(m);
                    self.stop_turn(client);
                    return false;
                }
                // Idle: the two-press exit. The first Ctrl+C clears the line and
                // shows `再按 Ctrl+C 退出` below the box; a second press while it is
                // up quits; once it has expired the next press is a fresh first one.
                return m.cancel_idle();
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
                        // A click inside an `[Image #N]` marker opens the picture
                        // in the desktop viewer rather than moving the caret — the
                        // marker is the picture, so clicking it is asking to see it.
                        // Only when the bytes are in hand; otherwise it is ordinary
                        // text and the click is a caret like any other.
                        if let Some(n) = crate::attach::marker_at_offset(&m.input, at) {
                            if let Some(image) = m.attachments.image_at(n).cloned() {
                                drop(m);
                                self.preview_image(n, Some(image));
                                return false;
                            }
                        }
                        m.caret = at;
                        return false;
                    }
                }
                drop(m);
                let Some((id, kind)) = self.host.block_at(x, y) else {
                    return false;
                };
                // A click on a sent message that carries a picture opens the
                // picture, the same as clicking its marker in the composer. The
                // hit map is per-row, not per-cell, so the block-level answer —
                // open the image the message holds — is the one available here;
                // a message is one screenshot in the overwhelmingly common case.
                let markers = self.host.image_markers_at(id);
                for &n in &markers {
                    let image = self
                        .host
                        .moment
                        .read()
                        .expect("moment poisoned")
                        .attachments
                        .image_at(n)
                        .cloned();
                    if let Some(image) = image {
                        self.preview_image(n, Some(image));
                        return false;
                    }
                }
                // A block that carries a picture is an image message, and the only
                // reason a user line is clickable at all — so the click is an
                // open-the-picture gesture, never a fold. When the bytes are gone
                // (a `resume`d session's gallery is empty, or the text merely
                // contains the `[Image #N]` characters) it is a no-op: folding a
                // person's own message on click would be a surprise. Only blocks
                // with no picture (a tool call, a reasoning block) fall through to
                // the fold below.
                if !markers.is_empty() {
                    return false;
                }
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
                    t(Msg::MouseTaken).into_owned()
                } else {
                    t(Msg::MouseHandedBack).into_owned()
                };
                self.say(&text);
                return false;
            }
            Action::ToggleSettings => {
                drop(m);
                let _ = self.host.toggle_settings();
                // The rows are read when the panel opens, not once at start-up:
                // a file edited behind the screen's back is whatever the file
                // says, and a list frozen at launch would quietly lie.
                if self.host.settings_open() {
                    self.refresh_settings();
                    self.fetch_usage();
                    self.fetch_status();
                }
                return false;
            }
            // Put another session on screen. The lock goes first: `switch_to`
            // draws that session and writes the moment, which is a lock this
            // guard still holds.
            Action::ToggleProviders => {
                drop(m);
                // Refused rather than silently opening a panel with no module
                // to draw it: a panel that is "up" but unmounted would take the
                // composer's rows and every key, and show nothing for either.
                if !self.host.toggle_providers() {
                    self.say(&t(Msg::NoProviderPanel));
                    return false;
                }
                // Read when the panel opens, not once at start-up: a file edited
                // behind the screen's back is whatever the file says, and a list
                // frozen at launch would quietly lie.
                if self.host.providers_open() {
                    self.refresh_providers();
                }
                return false;
            }
            // `/model`: the same providers panel, opened straight onto its model
            // list. One surface for switching and editing models, not a second
            // popup — mirrors `Action::ToggleProviders`'s refusal and refresh.
            Action::OpenModels => {
                drop(m);
                if !self.host.open_providers_on_models() {
                    self.say(&t(Msg::NoProviderPanel));
                    return false;
                }
                if self.host.providers_open() {
                    self.refresh_providers();
                }
                return false;
            }
            Action::TogglePlugins => {
                drop(m);
                // Refused rather than silently opening a panel with no module to
                // draw it, the same as the providers one.
                if !self.host.toggle_plugins() {
                    self.say(&t(Msg::NoPluginPanel));
                    return false;
                }
                // Read when the panel opens: what is under `plugins/` can be
                // changed by `atomcode plugin` in another terminal, and a list
                // frozen at launch would quietly lie.
                if self.host.plugins_open() {
                    self.refresh_plugins();
                }
                return false;
            }
            Action::ToggleTools => {
                drop(m);
                if !self.host.toggle_tools() {
                    self.say(&t(Msg::NoToolPanel));
                    return false;
                }
                // Read when the panel opens, never per frame: what the model can
                // call is a fact of the running tree, and an MCP server that
                // finished connecting since last time has changed it.
                if self.host.tools_open() {
                    self.refresh_tools();
                }
                return false;
            }
            Action::ToggleMcp => {
                drop(m);
                // Refused rather than silently opening a panel with no module to
                // draw it, the same as the three above.
                if !self.host.toggle_mcp() {
                    self.say(&t(Msg::McpPanelUnavailable));
                    return false;
                }
                // Read when the panel opens, never per frame: which servers exist,
                // and whether this project is trusted, is a fact of the running
                // tree that changes without anything on this screen doing it.
                if self.host.mcp_open() {
                    self.refresh_mcp();
                }
                return false;
            }
            Action::ToggleRewind => {
                drop(m);
                self.toggle_rewind_panel();
                return false;
            }
            // `/resume` brings the panel up on the sessions it already fetched:
            // put them in the moment first, then open the panel over them.
            Action::OpenResume(view) => {
                drop(m);
                self.host.show_resume(view);
                if !self.host.open_resume() {
                    self.say(&t(Msg::NoResumePanel));
                    return false;
                }
                // 开着就去问第一行聊了什么:等人按一下方向键才显示,等于第一眼
                // 看到的永远是空的。
                self.fetch_resume_preview();
                return false;
            }
            Action::LookAt(session) => {
                drop(m);
                self.switch_to(&session);
                return false;
            }
            // The cycle key's action, so a modal or a command that asks for the
            // same step lands in the same place the key does.
            Action::CycleMode => {
                drop(m);
                return self.cycle_mode();
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
            // Double- and triple-click: put the word (or the line) up as a
            // selection and copy it in one gesture, the way a terminal does.
            // `compose` reads the moment, so the guard goes first — then the
            // selection is set back on it, the same drop/compose dance as
            // `CopySelection`.
            Action::SelectWord(x, y) => {
                drop(m);
                let frame = self.host.compose(self.surface.size());
                if let Some(sel) = frame.word_at(x, y) {
                    let text = frame.selected_text(&sel);
                    self.host.moment.write().expect("moment poisoned").selection = Some(sel);
                    if !text.is_empty() {
                        self.surface.copy(&text);
                    }
                }
                return false;
            }
            Action::SelectLine(_x, y) => {
                drop(m);
                let frame = self.host.compose(self.surface.size());
                if let Some(sel) = frame.line_at(y) {
                    let text = frame.selected_text(&sel);
                    self.host.moment.write().expect("moment poisoned").selection = Some(sel);
                    if !text.is_empty() {
                        self.surface.copy(&text);
                    }
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
                    // Confirm on the tip row, the way the right-click `copy` menu
                    // item does — the auto-copy of a drag-release is still a copy,
                    // and it must say so on the gesture itself rather than leave
                    // the person to copy a second time to learn it worked.
                    self.host.say(t(Msg::CopiedSelection).into_owned(), false);
                }
                return false;
            }
            // One layer at a time: the selection (above), then what is typed,
            // then the turn. Clearing a draft you were still writing is
            // annoying; losing it because you wanted to stop the model is
            // worse, which is why ctrl-c stays `Cancel` and only stops.
            Action::Escape => {
                // While a turn is in flight, one Escape stops it — no double-tap
                // when there is something running to stop. If the composer is
                // empty, the prompt that was running comes back into it, caret at
                // the end, ready to edit and resend; if you had already started
                // typing something else, that is left exactly as it was, caret
                // where it sat.
                //
                // In flight by either account: this screen's own (`activity`),
                // or the agent's — it says it is working, or something sent to it
                // has not been taken yet. The second is what knows about a turn
                // this screen did not start (`/init`, `/setup`, a member's report
                // waking the lead) and the instant between a send and its
                // `TurnStarted`; without it esc armed a double-tap there.
                if m.turn_in_flight() || !client.settled() {
                    m.disarm_escape();
                    // Marked here, on the key, not on `AgentEvent::Cancelled`: the
                    // kernel cancels a turn for its own reasons too (a mid-turn
                    // model switch reconfigures and cancels), and only a stop the
                    // person asked for is theirs to be told about. The note waits
                    // for the turn to be idle before it draws (see `modules::input`),
                    // so it never overlaps the turn it closes.
                    m.interrupted = true;
                    if m.input.is_empty() {
                        if let Some(sent) = m.last_sent.clone() {
                            m.input = sent;
                            m.caret = m.input.len();
                            m.history_at = None;
                            m.draft.clear();
                        }
                    }
                    drop(m);
                    self.stop_turn(client);
                    return false;
                }
                // Idle. Every step is a double-tap inside `ESC_AGAIN_MS`, and the
                // second tap acts on whatever the field holds now: a field with
                // text clears, an empty one pulls the rewind panel up. So text
                // takes two taps to clear and two more to reach rewind; an empty
                // field takes two to reach rewind.
                if m.escape_again() {
                    m.disarm_escape();
                    if !m.input.is_empty() {
                        m.draft.clear();
                        m.history_at = None;
                        m.input.clear();
                        m.caret = 0;
                        return false;
                    }
                    drop(m);
                    self.toggle_rewind_panel();
                    return false;
                }
                m.arm_escape();
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
            Action::SetToolOutput(mode) => {
                drop(m);
                self.host
                    .presentation
                    .write()
                    .expect("presentation poisoned")
                    .set_tool_output(mode);
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

    /// What has been typed after the slash, while a command is being named.
    ///
    /// `None` when the line is not a command being typed: no slash, or one with
    /// a space after it (the name is settled and the argument has begun). One
    /// reader for the two questions that depend on it — what the menu should
    /// list, and whether the return key has been told anything yet — so those
    /// two cannot disagree about what is on the line.
    fn slash_prefix(&self) -> Option<String> {
        let typed = self
            .host
            .moment
            .read()
            .expect("moment poisoned")
            .input
            .clone();
        let rest = typed.strip_prefix('/')?;
        if rest.contains(char::is_whitespace) {
            return None;
        }
        Some(rest.to_string())
    }

    /// Recompute the slash menu from what is being typed.
    ///
    /// The list's *contents* are a question about commands and this is the row
    /// that owns the composer, so the registry is read here; where the panel is
    /// drawn and which row is lit are the host's. That split is what makes the
    /// menu's cursor survive: this recomputes the items, and the host resets the
    /// cursor to the first of them — the row that was lit a keystroke ago is not
    /// the same command once the list has narrowed.
    fn refresh_menu(&self) {
        let menu = match self.slash_prefix() {
            Some(rest) => {
                let matches = self.host.commands.matching(&rest);
                // The agent's description, for a command that expands its closed
                // set (below): the effort in force marks the ✓ row, and the
                // model's OWN levels are the rows offered. Read only when
                // something on screen actually expands, so the common menus
                // (`/help`, `/compact`) do not clone the description each
                // keystroke.
                let described = matches
                    .iter()
                    .any(|c| !c.options.is_empty() && c.answers_to(&rest))
                    .then(|| self.client.described())
                    .flatten();
                let current_effort = described
                    .as_ref()
                    .and_then(|d| d.reasoning_effort)
                    .map(|level| level.as_str().to_string());
                let mut items: Vec<crate::menu::Item> = Vec::new();
                for c in matches {
                    // A command with a closed set of values, once fully named, is
                    // not one row but one row per value: the menu's own way to
                    // pick an argument, in place of a modal. `{name} {value}`
                    // dispatches the command the row stands for.
                    if !c.options.is_empty() && c.answers_to(&rest) {
                        // `/effort`'s values are the MODEL's, not a fixed set: a
                        // model with no reasoning control offers only `default`,
                        // a restricted one offers exactly its declared levels.
                        // Until the model has described itself the static set is
                        // the fallback. Any other closed-set command uses its own.
                        let opts = if c.name == "effort" {
                            match described.as_ref() {
                                Some(d) => effort_menu_options(&d.effort_levels),
                                None => c.options.clone(),
                            }
                        } else {
                            c.options.clone()
                        };
                        for opt in &opts {
                            // Effort's `default` is "leave it to the endpoint",
                            // i.e. no level set — so `default` is the marked row
                            // exactly when nothing is in force.
                            let active = match &current_effort {
                                Some(level) => level == opt.value.as_ref(),
                                None => opt.value == "default",
                            };
                            let shown = format!("{} {}", c.name, opt.value);
                            // The mark goes through the capability table: on a
                            // terminal that draws `✓` as a box, this menu is
                            // where a person reads which level is in force
                            // (`gates/tui-layers.sh`, literal_glyphs).
                            let label = if active {
                                format!("{shown} {}", self.surface.caps().g(crate::caps::Glyph::Ok))
                            } else {
                                shown.clone()
                            };
                            items.push(
                                crate::menu::Item::new(shown, label).about(opt.about.to_string()),
                            );
                        }
                        continue;
                    }
                    // The label shows the aliases (`session (new)`); the value
                    // inserted / dispatched stays the canonical name.
                    let label = match &c.takes {
                        Some(t) => format!("{} {t}", c.display_name()),
                        None => c.display_name(),
                    };
                    items.push(
                        crate::menu::Item::new(c.name.to_string(), label)
                            .about(c.about.to_string()),
                    );
                }
                items
            }
            // Not a command being named. It may still be a path being typed
            // after `@` — the same discovery surface the slash menu is, for the
            // other thing people type by name and get wrong. It lists and
            // nothing more; finishing the word is still the typist's.
            None => {
                let (typed, cwd) = {
                    let m = self.host.moment.read().expect("moment poisoned");
                    (m.input.clone(), m.cwd.clone())
                };
                match crate::text::being_pathed(&typed) {
                    Some(prefix) => paths_under(&cwd, prefix),
                    None => Vec::new(),
                }
            }
        };
        self.host.set_menu(menu);
    }

    /// Route one key to the slash menu. Returns `true` to quit.
    ///
    /// Only the keys the list owns come here — [`crate::menu::Slash::owns`] —
    /// and the caller has already asked that. Everything else falls through to
    /// the composer, which is what lets `/com` keep narrowing while the highlight
    /// sits on its first row.
    ///
    /// **Tab completes and enter takes.** Tab puts the lit name on the line so
    /// its argument can be typed; enter dispatches the lit name outright, and a
    /// command that needs an argument answers with a panel to pick it from —
    /// which is where the second level of a command comes from, not from this
    /// screen knowing what the arguments are.
    fn slash_menu_key(&self, press: crate::surface::KeyPress, client: &AgentClient) -> bool {
        use crate::surface::Key;
        match press.key {
            Key::Up => {
                let _ = self.host.menu_move_by(-1);
            }
            Key::Down => {
                let _ = self.host.menu_move_by(1);
            }
            // Esc puts the list away and leaves what is typed alone. It is not
            // "clear the line" — the slash is still there, and the menu comes
            // back on the next keystroke that changes it. The one way to say
            // "not that list, this line" without losing the line.
            Key::Esc => {
                self.host.close_menu();
            }
            Key::Tab => {
                if let Some(name) = self.host.menu_selected() {
                    self.complete_command(&name);
                }
            }
            Key::Enter => {
                // Taken, always, whenever the list is up. The lit row is on
                // screen and says what it would do, and the return key acts on
                // what the screen shows — the same contract the question panel
                // keeps ("a stray return takes what the screen shows it would
                // take, never a hidden default").
                //
                // It used to refuse a bare `/`, on the reasoning that its first
                // row is `cancel-all` and nothing had been aimed at. That was
                // wrong twice over: it made the key *dead* — the list keeps
                // enter, so a refusal is not a fall-through to the composer, it
                // is a keystroke that does nothing at all and is indistinguishable
                // from a freeze — and it invented a rule the rest of this UI does
                // not have. If a row should not be taken, the row should not be
                // offered; a bare `/` offering everything is the list doing its
                // job.
                if let Some(name) = self.host.menu_selected() {
                    return self.take_command(&name, client);
                }
            }
            _ => {}
        }
        false
    }

    /// Take the command on the lit row — what enter and a press on a row share.
    ///
    /// One implementation, so the return key and a click cannot disagree about
    /// what the lit row means. The command is dispatched **by name with no
    /// argument**: what a command wants after its name is the command's own
    /// business. This screen knows how to lay a list out; it does not know that
    /// `/effort` takes a level.
    ///
    /// The one exception is a command with a closed set of values: taking its
    /// row opens that set — one row per value — rather than running it bare,
    /// because the pick a person came for is a level below. That is [`complete`]
    /// followed by the menu recomputing into the values, the same as tab. A row
    /// that already names a value (`effort high`, from that expansion) has no
    /// options of its own and runs, which is how the level is chosen.
    ///
    /// [`complete`]: Self::complete_command
    ///
    /// The line is cleared first, because the line is where the *prefix* was —
    /// leaving `/comp` behind a command that just ran is a composer still holding
    /// half a name, and it would recompute the menu from it.
    fn take_command(&self, name: &str, client: &AgentClient) -> bool {
        // A command with a closed set opens that set; one whose free-text
        // argument is required and has no bare form completes onto the line for
        // the argument to be typed (`/rename `). Both are `complete`, not a bare
        // dispatch that could only answer "needs an argument".
        if self
            .host
            .commands
            .find(name)
            .is_some_and(|c| !c.options.is_empty() || c.require_arg)
        {
            self.complete_command(name);
            return false;
        }
        {
            let mut m = self.host.moment.write().expect("moment poisoned");
            m.input.clear();
            m.caret = 0;
            m.history_at = None;
        }
        self.host.close_menu();
        if name == "quit" || name == "exit" {
            return self.act(crate::keymap::Action::Quit, client);
        }
        self.run_command(&format!("/{name}"));
        false
    }

    /// Put a command's name on the line, with a space if it wants an argument.
    ///
    /// The registry is asked rather than the menu: whether `/effort` wants
    /// something after it is a fact about the command, and the menu's label
    /// only *shows* it. A name completed without that space leaves the caret
    /// against the name, which is where a person would type the space
    /// themselves — the difference between a completion and a spell-check.
    fn complete_command(&self, name: &str) {
        let takes = self
            .host
            .commands
            .find(name)
            .is_some_and(|c| c.takes.is_some());
        let text = if takes {
            format!("/{name} ")
        } else {
            format!("/{name}")
        };
        let mut m = self.host.moment.write().expect("moment poisoned");
        m.caret = text.len();
        m.input = text;
        m.history_at = None;
        drop(m);
        self.refresh_menu();
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
            crate::menu::Item::new("copy", t(Msg::MenuCopySelection))
                .about(t(Msg::MenuCopySelectionAbout))
        } else {
            crate::menu::Item::new("copy", t(Msg::MenuCopyAll)).about(t(Msg::MenuCopyAllAbout))
        };
        let in_composer = self
            .host
            .field_rect()
            .is_some_and(|field| field.contains(x, y));
        let items = if in_composer {
            vec![
                copy,
                crate::menu::Item::new("paste", t(Msg::MenuPaste)).about(t(Msg::MenuPasteAbout)),
                crate::menu::Item::new("clear", t(Msg::MenuClear)).about(t(Msg::MenuClearAbout)),
                crate::menu::Item::new("send", t(Msg::MenuSend)).about(t(Msg::MenuSendAbout)),
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
                        self.host.say(t(Msg::CopiedSelection).into_owned(), false);
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
                            t(Msg::SelectionHasNoText)
                        } else {
                            t(Msg::NothingToCopy)
                        }
                        .into_owned(),
                        true,
                    );
                    return false;
                }
                self.surface.copy(&text);
                // The other front end's usage panel already says this.
                self.host.say(
                    crate::i18n::product::t(crate::i18n::product::Msg::UsageCopied).into_owned(),
                    false,
                );
                false
            }
            "paste" => {
                let Some(text) = self.surface.clipboard_text() else {
                    // The menu's own refusal, so it belongs on the tip row with
                    // the rest of them rather than in the conversation.
                    self.host
                        .say(t(Msg::ClipboardHasNoTextShort).into_owned(), true);
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
            deliver(&host, &keys, outcome);
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
        let Some(keys) = keys else { return };
        tokio::spawn(async move {
            let outcome = host.commands.dispatch(&value, &ctx).await;
            deliver(&host, &keys, outcome);
        });
    }
}

/// Ask for a frame.
///
/// The loop paints when something wakes it, and everything that wakes it today
/// is either a key or the connection. Work that changes what is on screen from
/// somewhere else — a login being polled behind a modal, a file being watched —
/// has nothing to ring, and its change sits there unpainted until the next
/// keystroke.
///
/// A seam rather than the wake channel itself: what is outside the loop needs
/// to say "this changed", not to reach into how the loop is built.
pub trait Repaint: Send + Sync {
    fn now(&self);
}

/// The screen's own: the same slot the loop fills when it starts.
///
/// Holding the slot rather than a sender is what lets this be provided when the
/// row mounts, which is when a capability map can see it. Before the loop is
/// up, asking for a frame does nothing — which is the honest answer: there is
/// no frame yet, and the first one is owed anyway.
struct Waker(Arc<Tui>);

impl Repaint for Waker {
    fn now(&self) {
        let sender = self.0.wake.lock().expect("wake poisoned").clone();
        if let Some(sender) = sender {
            let _ = sender.send(Wake::Fact);
        }
    }
}

/// Do what a command answered with.
///
/// One implementation for the two ways a command is reached — typed, and picked
/// out of a modal — because they drifted: the picked path handled `Said` and
/// `Refused` and **dropped the rest**, so a modal whose pick opened another
/// modal did nothing at all, silently. A wizard's last step is exactly that
/// shape, which is how this was found.
fn deliver(
    host: &Arc<crate::host::Host>,
    keys: &mpsc::UnboundedSender<Wake>,
    outcome: crate::command::Outcome,
) {
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
            // A command and a key share one implementation, so this is the same
            // path a keystroke takes.
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
}

/// Take the host's word for what a session is doing on its own.
///
/// A separate function because the loop it is called from cannot be reached
/// from a criterion, and "did the screen keep what the host pushed" is exactly
/// the thing worth asserting. Returns whether anything moved.
///
/// News about a session nobody is looking at is dropped: a member running a
/// goal must not overwrite the lead's status line.
pub fn took_autonomy(
    moment: &std::sync::RwLock<crate::moment::Moment>,
    session: &str,
    running: Option<atomcode_host_api::Running>,
) -> bool {
    let mut m = moment.write().expect("moment poisoned");
    if m.viewing != session && m.lead != session {
        return false;
    }
    if m.autonomy == running {
        return false;
    }
    m.autonomy = running;
    true
}

/// Take the host's word for how much this session may do without asking.
///
/// [`took_autonomy`]'s twin, and split out for the same reason: the loop it is
/// called from cannot be reached from a criterion, and "did the screen keep
/// what the host pushed" is exactly the thing worth asserting. Returns whether
/// anything moved.
///
/// News about a session nobody is looking at is dropped on the same terms: a
/// member switching its own mode must not relabel the lead's status row.
pub fn took_mode(
    moment: &std::sync::RwLock<crate::moment::Moment>,
    session: &str,
    mode: Option<atomcode_host_api::Mode>,
) -> bool {
    let mut m = moment.write().expect("moment poisoned");
    if m.viewing != session && m.lead != session {
        return false;
    }
    if m.mode == mode {
        return false;
    }
    m.mode = mode;
    true
}

/// Take the ghost into the field, if the caret is at the end and there is one.
///
/// `true` when it took something, which is the caller's cue that right meant
/// "accept" rather than "move".
fn accept_ghost(m: &mut crate::moment::Moment) -> bool {
    if m.caret != m.input.len() {
        return false;
    }
    let Some(rest) =
        crate::text::ghost(&m.input, &m.history, m.history_at.is_some()).map(str::to_string)
    else {
        return false;
    };
    m.input.push_str(&rest);
    m.caret = m.input.len();
    true
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

/// Whether a picture attached to this conversation has somewhere to go. `Err` is
/// the reason it does not, phrased for the person.
///
/// A picture reaches ANY mounted model: a vision model takes the bytes, and a
/// text-only one has them turned into a caption by the runtime's VL preprocessor
/// — a configured `vision_preprocessor_provider`, or the one `/codingplan`
/// auto-detects from the managed model list (the "default vision"). The paste is
/// therefore not refused for a text-only model the way it once was: whether the
/// caption succeeded, or there was no VL helper to make one, is the turn's
/// business, reported on send (the runtime clears the images and says so rather
/// than dropping them silently). Deciding it here — before the runtime is even
/// consulted — is what wrongly refused a text-only model that DID have a helper.
///
/// The one thing still refused is no model at all: a picture with no destination
/// whatsoever is not sent by omission.
fn images_reach_the_model(client: &AgentClient) -> Result<(), String> {
    match client.described() {
        Some(_) => Ok(()),
        None => Err(t(Msg::ModelUnknownForImages).into_owned()),
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
    // Empty: the bindings come from a row, the way the panels and the commands
    // do (`tui-keys-default`). A screen assembled here has the slot, not the
    // contents.
    let keys = Arc::new(Keys::new());
    (
        host.clone(),
        Tui {
            client: Arc::new(AgentClient::default()),
            host,
            keys,
            surface,
            ctx: Mutex::new(None),
            wake: Mutex::new(None),
            allowance_checked: Mutex::new(None),
            pressed_at: Mutex::new(None),
            click_streak: Mutex::new(None),
            members: Arc::new(Roster::default()),
            named: Mutex::new(None),
            image_files: Mutex::new(std::collections::HashMap::new()),
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
            "tui-keys",
            "tui-layout",
            // Both are provided by this row and were not declared: the roster
            // in `apply`, the repaint seam once the loop exists. An undeclared
            // provide is invisible to the capability map, which is what the
            // launcher's audit is for.
            "tui-team-roster",
            "tui-repaint",
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
            .provide::<KeysSvc>(tui.keys.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<AgentClientSvc>(tui.client.clone())
            .map_err(|e| e.to_string())?;
        // The same roster the screen keeps, handed to the tree so `/agents` can
        // list a member the team panel no longer has a row for.
        let _ = ctx
            .provide::<TeamRosterSvc>(tui.members.clone())
            .map_err(|e| e.to_string())?;
        let tui = Arc::new(tui);
        // The seam anything outside the loop asks for a frame through. Provided
        // here, with the screen itself behind it, so the slot is full from the
        // moment this row mounts; what it rings is filled in when the loop
        // starts.
        let _ = ctx
            .provide::<RepaintSvc>(Arc::new(Waker(tui.clone())) as Arc<dyn Repaint>)
            .map_err(|e| e.to_string())?;
        let _ = ctx.provide::<UiSvc>(tui).map_err(|e| e.to_string())?;
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
    /// What the terminal can do, when this build knows better than detection
    /// does. All three optional; anything left out is detected as before.
    ///
    /// For a fleet: an image whose `TERM` says `xterm` on emulators that do
    /// 24-bit colour states it once here rather than exporting a variable on
    /// every machine. `ATOMCODE_ASCII` remains, and still wins — it is the
    /// per-session escape hatch, and a person on one bad terminal must be able
    /// to override the tree they share.
    #[serde(default)]
    unicode: Option<bool>,
    #[serde(default)]
    colors: Option<String>,
    #[serde(default)]
    cell_background: Option<bool>,
}

fn yes() -> bool {
    true
}

/// The configured palette (`None` means "ask the terminal"), and whether to
/// report the pointer.
fn surface_row(
    config: &Value,
) -> Result<(Option<crate::theme::Theme>, bool, crate::caps::Overrides), String> {
    let row: SurfaceRow = if config.is_null() {
        SurfaceRow {
            theme: None,
            mouse: true,
            unicode: None,
            colors: None,
            cell_background: None,
        }
    } else {
        serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
    };
    let colors = match row.colors.as_deref() {
        None => None,
        Some("none") => Some(crate::caps::Colors::None),
        Some("16") => Some(crate::caps::Colors::Ansi16),
        Some("256") => Some(crate::caps::Colors::Ansi256),
        Some("true") => Some(crate::caps::Colors::True),
        Some(other) => return Err(format!("colors `{other}` is not none, 16, 256 or true")),
    };
    let overrides = crate::caps::Overrides {
        unicode: row.unicode,
        colors,
        cell_background: row.cell_background,
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
    Ok((theme, mouse, overrides))
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
        let (theme, mouse, overrides) = surface_row(config)?;
        let term = Terminal::enter(theme, mouse, overrides)
            .map_err(|e| format!("cannot take the terminal: {e}"))?;
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
mod effort_menu_tests {
    use super::effort_menu_options;

    #[test]
    fn a_model_with_no_reasoning_control_offers_only_default() {
        // Empty levels — the description of a model that cannot reason — is the
        // whole point of reading them off the model: the menu collapses to the
        // one row that means "leave it to the endpoint".
        let opts = effort_menu_options(&[]);
        let values: Vec<&str> = opts.iter().map(|o| o.value.as_ref()).collect();
        assert_eq!(values, ["default"], "only `default`: {values:?}");
    }

    #[test]
    fn a_restricted_model_offers_exactly_its_levels_then_default() {
        // The model's OWN set, in its order, with `default` appended — not the
        // full ladder. A model that supports `low`/`high` must not offer
        // `medium`/`xhigh`/`max`.
        let opts = effort_menu_options(&["low".to_string(), "high".to_string()]);
        let values: Vec<&str> = opts.iter().map(|o| o.value.as_ref()).collect();
        assert_eq!(values, ["low", "high", "default"], "{values:?}");
    }
}

#[cfg(test)]
mod surface_row_tests {
    use super::surface_row;

    /// `[ui] mouse = false` hands the pointer back to the terminal, so click-drag
    /// does the terminal's own selection. `false && env` is false whatever the
    /// `ATOMCODE_NO_MOUSE` override is, so this does not read the environment.
    #[test]
    fn mouse_off_hands_the_pointer_back_to_the_terminal() {
        let (_, on, _) =
            surface_row(&serde_json::json!({ "mouse": false })).expect("mouse=false parses");
        assert!(!on, "mouse=false must hand the pointer back");
        // A realistic surface config (theme + mouse together) still parses, and
        // its unknown-field guard does not choke on the pair.
        let (_, on, _) = surface_row(&serde_json::json!({ "theme": "dark", "mouse": false }))
            .expect("theme + mouse parse");
        assert!(!on);
    }
}

#[cfg(test)]
mod history_tests {
    use super::{accept_ghost, recall_back, recall_forward};
    use crate::moment::Moment;

    fn said(entries: &[&str], typing: &str) -> Moment {
        let mut m = Moment::default();
        m.history = entries.iter().map(|s| s.to_string()).collect();
        m.input = typing.to_string();
        m.caret = m.input.len();
        m
    }

    /// Right at the end of the line takes the suggestion; everywhere else it is
    /// still a caret move. A key that sometimes eats the caret gesture without
    /// saying so is worse than no suggestion.
    #[test]
    fn right_takes_the_ghost_only_at_the_end_of_the_line() {
        let mut m = said(&["cargo test", "cargo nextest run"], "cargo ");
        assert!(accept_ghost(&mut m), "there is one to take");
        assert_eq!(m.input, "cargo nextest run");
        assert_eq!(m.caret, m.input.len());

        // Nothing left: right goes back to being a caret move.
        assert!(!accept_ghost(&mut m));

        // Mid-line, right moves rather than completing.
        let mut m = said(&["cargo nextest run"], "cargo ");
        m.caret = 2;
        assert!(!accept_ghost(&mut m));
        assert_eq!(m.input, "cargo ");

        // While arrowing the history the field already shows an entry.
        let mut m = said(&["cargo nextest run"], "cargo ");
        m.history_at = Some(0);
        assert!(!accept_ghost(&mut m));
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
mod autonomy_tests {
    use super::took_autonomy;
    use crate::moment::Moment;
    use std::sync::RwLock;

    fn running(round: u32) -> atomcode_host_api::Running {
        atomcode_host_api::Running {
            kind: "goal".into(),
            what: "测试全绿".into(),
            round,
            of: Some(40),
            elapsed_secs: 90,
            paused: None,
        }
    }

    /// A status line can only move on its own if something pushes to it. The
    /// host announces each round; the screen keeps the latest.
    ///
    /// The three answers that matter are different: a new round moved
    /// something, the same round did not (a screen that redrew on every
    /// identical announcement would blink for no reason), and the end clears it
    /// rather than leaving the last round on screen forever.
    #[test]
    fn the_screen_keeps_what_the_host_pushes_about_a_running_goal() {
        let moment = RwLock::new(Moment {
            lead: "lead".into(),
            viewing: "lead".into(),
            ..Moment::default()
        });

        assert!(took_autonomy(&moment, "lead", Some(running(3))));
        assert_eq!(
            moment.read().unwrap().autonomy.as_ref().map(|r| r.round),
            Some(3)
        );
        // The same news again is not news.
        assert!(!took_autonomy(&moment, "lead", Some(running(3))));
        assert!(took_autonomy(&moment, "lead", Some(running(4))));
        // Over: the line goes away rather than freezing on round 4.
        assert!(took_autonomy(&moment, "lead", None));
        assert!(moment.read().unwrap().autonomy.is_none());
    }

    /// A member running a goal must not write on the lead's line.
    ///
    /// Sessions are announced by name for this reason: with a team, several are
    /// running at once, and what is drawn belongs to the one on screen.
    #[test]
    fn news_about_a_session_nobody_is_watching_is_dropped() {
        let moment = RwLock::new(Moment {
            lead: "lead".into(),
            viewing: "lead".into(),
            ..Moment::default()
        });
        assert!(!took_autonomy(&moment, "member-2", Some(running(9))));
        assert!(moment.read().unwrap().autonomy.is_none());

        // Switch to that member and it is that member's line.
        moment.write().unwrap().viewing = "member-2".into();
        assert!(took_autonomy(&moment, "member-2", Some(running(9))));
        assert_eq!(
            moment.read().unwrap().autonomy.as_ref().map(|r| r.round),
            Some(9)
        );
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
                !host.secret_key(press(Key::Char(c))),
                "typing does not close the prompt"
            );
        }
    }

    /// What the composer is showing right now, drawn.
    fn field(host: &crate::host::Host) -> String {
        host.compose((60, 12))
            .part(input::ID)
            .expect("the field is on screen")
            .lines
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A password `sudo` asks for reaches the screen **on the composer's line**,
    /// and what the person types reaches the one waiting for it — which is what
    /// makes a `sudo` inside a tool call finish instead of hanging on a tty this
    /// screen owns (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md`
    /// P0-1).
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
        assert!(host.secret_waiting(), "the prompt is up");
        let asked = field(&host);
        assert!(
            asked.contains("password for lichao"),
            "asked in the words the program used, on the line: {asked:?}"
        );

        typed(&host, "hunter2");
        // One mask glyph per character and not one of the characters: the field
        // is the composer's, and what is typed into it here is not.
        let masked = field(&host);
        assert_eq!(
            masked.matches("•").count(),
            7,
            "a keystroke shows, as a mask: {masked:?}"
        );
        assert!(!masked.contains("hunter2"), "{masked:?}");
        assert!(
            host.secret_key(press(Key::Enter)),
            "enter closes the prompt"
        );
        assert_eq!(answered.await.unwrap().as_deref(), Some("hunter2"));
        assert!(
            !host.secret_waiting(),
            "and the field is the composer's again"
        );
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
        while !host.secret_waiting() {
            tokio::task::yield_now().await;
        }
        typed(&host, "half a password");
        assert!(host.secret_key(press(Key::Esc)), "esc closes the prompt");
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

#[cfg(test)]
mod mcp_panel_tests {
    use super::*;
    use crate::module::Mounted;
    use crate::surface::{Key, KeyPress};
    use atomcode_plexus::{App, ConfigTree, PluginRegistry};

    /// A port with one server, a page for it, and an action that lands.
    struct FakeMcp;

    fn server(state: crate::mcp::McpState) -> crate::mcp::McpRow {
        crate::mcp::McpRow {
            name: "figma".to_string(),
            state,
            source: "project".to_string(),
            tool_count: 3,
            config_path: None,
        }
    }

    #[async_trait]
    impl crate::mcp::Mcp for FakeMcp {
        async fn list(&self) -> Result<crate::mcp::McpView, String> {
            Ok(crate::mcp::McpView::new(vec![server(
                crate::mcp::McpState::NeedsAuthentication,
            )]))
        }

        async fn detail(&self, server: &str) -> Result<crate::mcp::McpDetail, String> {
            Ok(crate::mcp::McpDetail {
                name: server.to_string(),
                state: crate::mcp::McpState::NeedsAuthentication,
                source: "project".to_string(),
                transport: crate::mcp::Transport::Http {
                    url: "https://mcp.figma.com/mcp".to_string(),
                },
                auth: crate::mcp::Auth::OAuth {
                    authenticated: false,
                },
                tool_count: 3,
                config_path: Some("/tmp/example/.mcp.json".to_string()),
            })
        }

        /// The action lands: the server comes back connected, which is the one
        /// difference the screen can see for itself.
        async fn act(
            &self,
            _server: &str,
            _action: crate::mcp::Action,
        ) -> Result<crate::mcp::McpView, String> {
            Ok(crate::mcp::McpView::new(vec![server(
                crate::mcp::McpState::Connected,
            )]))
        }
    }

    /// 两台服务器,而且**记下**每一次动作是冲着谁发的——判据要看的正是这个:
    /// 「动作发对了」不能只看屏幕上画的是谁,要看真发出去的那一份。
    struct TwoServers {
        asked: Arc<std::sync::Mutex<Vec<(String, crate::mcp::Action)>>>,
    }

    fn named(name: &str, state: crate::mcp::McpState) -> crate::mcp::McpRow {
        crate::mcp::McpRow {
            name: name.to_string(),
            state,
            source: "project".to_string(),
            tool_count: 1,
            config_path: None,
        }
    }

    fn page_of(name: &str) -> crate::mcp::McpDetail {
        crate::mcp::McpDetail {
            name: name.to_string(),
            state: crate::mcp::McpState::NeedsAuthentication,
            source: "project".to_string(),
            transport: crate::mcp::Transport::Http {
                url: "https://example.invalid/mcp".to_string(),
            },
            auth: crate::mcp::Auth::OAuth {
                authenticated: false,
            },
            tool_count: 1,
            config_path: None,
        }
    }

    #[async_trait]
    impl crate::mcp::Mcp for TwoServers {
        async fn list(&self) -> Result<crate::mcp::McpView, String> {
            Ok(crate::mcp::McpView::new(vec![
                named("alpha", crate::mcp::McpState::NeedsAuthentication),
                named("beta", crate::mcp::McpState::NeedsAuthentication),
            ]))
        }

        async fn detail(&self, server: &str) -> Result<crate::mcp::McpDetail, String> {
            Ok(page_of(server))
        }

        async fn act(
            &self,
            server: &str,
            action: crate::mcp::Action,
        ) -> Result<crate::mcp::McpView, String> {
            self.asked
                .lock()
                .expect("asked poisoned")
                .push((server.to_string(), action));
            Ok(crate::mcp::McpView::new(vec![named(
                server,
                crate::mcp::McpState::Connected,
            )]))
        }
    }

    /// 一个上下文,端口就是给的那一个。
    fn with_port_of(tui: &Tui, port: Arc<dyn crate::mcp::Mcp>) {
        let app = App::new(PluginRegistry::new(), ConfigTree::default());
        let _ = app.context().provide::<McpSvc>(port);
        *tui.ctx.lock().expect("ctx poisoned") = Some(app.context());
    }

    /// 面板此刻在看**哪一台**。
    fn looked_at(host: &Arc<Host>) -> Option<String> {
        let m = host.moment.read().expect("moment poisoned");
        m.mcp_panel.as_ref().and_then(|p| p.detail_for.clone())
    }

    /// A screen with the MCP panel's module mounted and a wake channel on it:
    /// what a bare `/mcp` needs, plus the door the answer comes back through.
    fn screen() -> (Arc<Host>, Tui, mpsc::UnboundedReceiver<Wake>) {
        let (host, tui) = assemble(Headless::new(80, 24));
        host.modules
            .add_view(Arc::new(Mounted::<crate::modules::mcp::Mcp>::new()))
            .unwrap();
        let (wake, woken) = mpsc::unbounded_channel();
        *tui.wake.lock().expect("wake poisoned") = Some(wake);
        (host, tui, woken)
    }

    /// Fill the seam the launcher's row fills, the way the loop hands it over.
    fn with_port(tui: &Tui) {
        let app = App::new(PluginRegistry::new(), ConfigTree::default());
        let _ = app.context().provide::<McpSvc>(Arc::new(FakeMcp));
        *tui.ctx.lock().expect("ctx poisoned") = Some(app.context());
    }

    /// A context with nothing in it: the screen is up, the seam was never filled.
    fn without_port(tui: &Tui) {
        let app = App::new(PluginRegistry::new(), ConfigTree::default());
        *tui.ctx.lock().expect("ctx poisoned") = Some(app.context());
    }

    /// A round trip is on its own task, so the test waits for the same thing the
    /// loop does: the wake it sends when it is done.
    async fn landed(woken: &mut mpsc::UnboundedReceiver<Wake>) {
        let woke = tokio::time::timeout(std::time::Duration::from_secs(5), woken.recv()).await;
        assert!(
            matches!(woke, Ok(Some(Wake::Fact))),
            "the round trip came back and said so"
        );
    }

    fn note(host: &Arc<Host>) -> Option<String> {
        let m = host.moment.read().expect("moment poisoned");
        m.mcp_panel.as_ref().and_then(|panel| panel.note.clone())
    }

    /// A port whose sign-in waits until it is cancelled, and counts the cancels.
    struct SlowSignIn {
        cancels: Arc<std::sync::atomic::AtomicUsize>,
        stopped: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl crate::mcp::Mcp for SlowSignIn {
        async fn list(&self) -> Result<crate::mcp::McpView, String> {
            FakeMcp.list().await
        }

        async fn detail(&self, server: &str) -> Result<crate::mcp::McpDetail, String> {
            FakeMcp.detail(server).await
        }

        /// Waits the way a browser sign-in does, until it is told to stop.
        async fn act(
            &self,
            _server: &str,
            _action: crate::mcp::Action,
        ) -> Result<crate::mcp::McpView, String> {
            self.stopped.notified().await;
            Err("认证已取消".to_string())
        }

        fn cancel(&self) {
            self.cancels
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.stopped.notify_one();
        }
    }

    /// `Esc` during a sign-in stops the sign-in, and the panel stays to say so.
    ///
    /// Found at a real terminal: the legend said `Esc 取消`, and `Esc` only put
    /// the panel away — the sign-in went on waiting on the browser, and its end
    /// was said to nobody. The first `Esc` has to reach the port's `cancel`, and
    /// the panel has to still be there when the sign-in answers.
    #[tokio::test]
    async fn escape_during_a_sign_in_cancels_it_and_the_panel_says_so() {
        let (host, tui, mut woken) = screen();
        let cancels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        with_port_of(
            &tui,
            Arc::new(SlowSignIn {
                cancels: Arc::clone(&cancels),
                stopped: Arc::new(tokio::sync::Notify::new()),
            }),
        );
        tui.act(Action::ToggleMcp, &tui.client);
        landed(&mut woken).await;
        tui.run_mcp_key(KeyPress::plain(Key::Enter));
        landed(&mut woken).await;
        // 1. 认证 — the one action on a server waiting for authentication.
        tui.run_mcp_key(KeyPress::plain(Key::Enter));

        tui.run_mcp_key(KeyPress::plain(Key::Esc));
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the first Esc reached the sign-in"
        );
        assert!(host.mcp_open(), "and the panel stayed to hear how it ended");
        landed(&mut woken).await;

        assert!(host.mcp_open(), "still up when the sign-in answered");
        assert_eq!(note(&host).as_deref(), Some("认证已取消"), "and it says so");
        let m = host.moment.read().expect("moment poisoned");
        assert!(
            m.mcp_panel
                .as_ref()
                .is_some_and(|panel| panel.busy.is_none()),
            "nothing is running any more"
        );
    }

    /// The command half is in `commands.rs` — it can only ask. This is the half
    /// that says the panel really rose, and on the directory the port answered.
    #[tokio::test]
    async fn the_mcp_panel_opens_on_the_directory_the_port_answered() {
        let (host, tui, mut woken) = screen();
        with_port(&tui);

        assert!(
            !tui.act(Action::ToggleMcp, &tui.client),
            "opening a panel is not quitting"
        );
        assert!(host.mcp_open(), "the panel is up, not a printed list");
        landed(&mut woken).await;

        let m = host.moment.read().expect("moment poisoned");
        let rows = m.mcp.rows();
        assert_eq!(rows.len(), 1, "what the port answered: {rows:?}");
        assert_eq!(rows[0].name, "figma");
        assert_eq!(rows[0].state, crate::mcp::McpState::NeedsAuthentication);
    }

    /// `Enter` on a server asks the port for its page, and the page is hung on
    /// the rows the panel already has — the list is not asked for again.
    #[tokio::test]
    async fn drilling_into_a_server_puts_its_detail_on_the_panel() {
        let (host, tui, mut woken) = screen();
        with_port(&tui);
        tui.act(Action::ToggleMcp, &tui.client);
        landed(&mut woken).await;

        assert!(
            tui.run_mcp_key(KeyPress::plain(Key::Enter)),
            "a key that asked for a page owes a frame"
        );
        landed(&mut woken).await;

        let m = host.moment.read().expect("moment poisoned");
        assert_eq!(
            m.mcp_panel.as_ref().map(|panel| panel.level),
            Some(crate::mcp::Level::Detail)
        );
        assert_eq!(
            m.mcp.detail().map(|detail| detail.name.as_str()),
            Some("figma")
        );
        assert_eq!(
            m.mcp.detail().map(|detail| detail.tool_count),
            Some(3),
            "and it is the page the port sent, not a stub built here"
        );
    }

    /// 从 A 的详情退回来、立刻进 B,再按下的动作必须是发给 **B** 的。
    ///
    /// 这条盯的是最贵的那种错:B 的回包还没到时,视图里压着的是 A 的那一份详情,
    /// 而屏幕上的动作表正是照它算出来的——照单执行就是拿 A 去停用/登出/取消信任。
    /// 判据不能只看屏幕上画着谁,要看真发出去的那一份,所以端口在这里记名。
    ///
    /// `current_thread` 是判据的一部分:只有单线程运行时,两次按键之间那个「回包
    /// 还没到」的窗口才是确定的,每一步都走同一条路。少了它,这条会时红时绿。
    #[tokio::test(flavor = "current_thread")]
    async fn an_action_from_the_page_never_reaches_the_server_it_was_not_asked_for() {
        let (host, tui, mut woken) = screen();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        with_port_of(
            &tui,
            Arc::new(TwoServers {
                asked: asked.clone(),
            }),
        );

        tui.act(Action::ToggleMcp, &tui.client);
        landed(&mut woken).await;

        // 钻进 alpha,回包到了。
        tui.run_mcp_key(KeyPress::plain(Key::Enter));
        landed(&mut woken).await;
        assert_eq!(looked_at(&host), Some("alpha".to_string()), "看的是 alpha");

        // 退回列表,走到 beta 上,再钻进去——**beta 的回包还没到**,视图里仍是
        // alpha 的那一页。
        tui.run_mcp_key(KeyPress::plain(Key::Esc));
        tui.run_mcp_key(KeyPress::plain(Key::Down));
        tui.run_mcp_key(KeyPress::plain(Key::Enter));
        assert_eq!(looked_at(&host), Some("beta".to_string()), "看的是 beta");

        // 就在这一页上按 Enter:动作一次都不许落到 alpha 身上。
        tui.run_mcp_key(KeyPress::plain(Key::Enter));

        // beta 的回包到了,这时才谈得上执行。
        landed(&mut woken).await;
        tui.run_mcp_key(KeyPress::plain(Key::Enter));
        landed(&mut woken).await;

        let asked = asked.lock().expect("asked poisoned").clone();
        assert!(
            !asked.iter().any(|(server, _)| server == "alpha"),
            "一次都不许打到 alpha 身上: {asked:?}"
        );
        assert_eq!(
            asked
                .iter()
                .map(|(server, _)| server.as_str())
                .collect::<Vec<_>>(),
            vec!["beta"],
            "而发出去的那一次是 beta 的"
        );
    }

    /// One action: it goes out over the seam, and what comes back is the
    /// directory **after** it — not this screen's guess at what it did.
    #[tokio::test]
    async fn an_action_goes_over_the_seam_and_the_refreshed_list_comes_back() {
        let (host, tui, mut woken) = screen();
        with_port(&tui);
        tui.act(Action::ToggleMcp, &tui.client);
        landed(&mut woken).await;
        // 先钻进去:动作在详情层,而光标落在第一个动作上(待认证 → 认证)。
        tui.run_mcp_key(KeyPress::plain(Key::Enter));
        landed(&mut woken).await;

        assert!(
            tui.run_mcp_key(KeyPress::plain(Key::Enter)),
            "a key that asked for an action owes a frame"
        );
        landed(&mut woken).await;

        let m = host.moment.read().expect("moment poisoned");
        assert!(
            m.mcp_panel
                .as_ref()
                .is_some_and(|panel| panel.busy.is_none()),
            "the round trip is over and the panel is not waiting any more"
        );
        assert_eq!(
            m.mcp.rows()[0].state,
            crate::mcp::McpState::Connected,
            "the list is what the port said afterwards"
        );
    }

    /// A build with the panel and no port behind it says so **on the panel**: an
    /// empty table would read as "no servers are configured", which is a
    /// statement about the person's own file (设计 §6).
    #[tokio::test]
    async fn a_screen_without_the_port_says_why_the_table_is_empty() {
        let (host, tui, _woken) = screen();
        without_port(&tui);

        tui.act(Action::ToggleMcp, &tui.client);

        assert!(host.mcp_open());
        assert_eq!(
            note(&host),
            Some(t(Msg::NoMcpPort).into_owned()),
            "the panel says what is missing"
        );
    }
}
