//! The rows: how this UI mounts into a harness config tree.
//!
//! The whole UI is one `ui` row plus whatever module rows the layout names.
//! Nothing here is privileged — remove `tui-mascot` from the tree and the cat
//! is gone, with no branch left behind anywhere.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::agent::{Agent, CreateAgent};
use atomcode_harness::events::SessionEventCommitted;
use atomcode_harness::plugins::handle::{spawn as spawn_driver, wire, Driven};
use atomcode_harness::seams::{LlmSvc, UiSvc, UserInterface, UserQuestionsSvc};
use atomcode_harness::session::{Committed, LoggedEvent, SeqNo};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
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
plexus_service!(AgentClientSvc => AgentClient, "tui-agent-client", Core, "The command channel to the agent this screen drives");

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

/// The screen's end of the handle protocol.
///
/// Everything this UI tells the agent goes through here as an
/// [`AgentCommand`] — the same eight the daemon, the SDK and the shipped TUI
/// speak. The UI does not run turns, does not decide what steering means and
/// does not order compaction behind the turn: the pump on the other end does,
/// once, under the differential gate. This row only turns keys into commands.
pub struct AgentClient {
    commands: mpsc::UnboundedSender<AgentCommand>,
    agent: Arc<Agent>,
}

impl AgentClient {
    /// The conversation this screen shows. Read-only from here: every change
    /// to it goes through a command.
    pub fn session(&self) -> Arc<atomcode_harness::session::SessionLog> {
        self.agent.session()
    }
    pub fn send(&self, text: String, images: Vec<atomcode_kernel::message::ImageContent>) {
        let _ = self
            .commands
            .send(AgentCommand::SendMessage { text, images });
    }
    pub fn cancel(&self) {
        let _ = self.commands.send(AgentCommand::Cancel);
    }
    pub fn compact(&self, focus: Option<String>) {
        let _ = self.commands.send(AgentCommand::Compact { focus });
    }
    pub fn shutdown(&self) {
        let _ = self.commands.send(AgentCommand::Shutdown);
    }
}

/// Lines the conversation moves per wheel notch.
///
/// One, not three. The terminal already sends one event per notch, so three
/// lines an event is three times the distance the hand asked for — and the
/// reason to reach for the wheel here is to study something that went past,
/// which is exactly when precision beats speed.
const WHEEL_LINES: i32 = 1;

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
    /// The agent said something about itself — over the handle, not the log.
    Event(AgentEvent),
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

/// The screen's subscription to its own session's log.
///
/// The rule is one line — every committed fact of OUR session reaches the
/// modules here and nowhere else — but the startup case is the whole reason
/// this is a type rather than a closure. A resumed process finds the log
/// already full: [`SessionLog::restore`] puts the history back **silently**,
/// and silently on purpose (the facts were broadcast once, in the process
/// that wrote them, and broadcasting them again would have every listener
/// treat them as news — persistence would append them a second time). So a
/// listener registered at startup hears only what happens *next*, and a
/// screen built that way comes back to an empty conversation with the whole
/// session sitting in the file. Measured: `atui --resume <id>` drew the
/// composer and the status line and nothing else.
///
/// [`SessionLog::restore`]: atomcode_harness::session::SessionLog::restore
struct Facts {
    host: Arc<Host>,
    state: Mutex<Feed>,
}

/// Whether the history has been folded yet, and how far.
enum Feed {
    /// Registered but not yet caught up. Facts queue rather than fold: the
    /// history is folded from the log in log order, and draining this after
    /// it is what keeps a fact that arrived mid-catch-up *behind* the
    /// history it follows. Folding it first would hand a module a tool
    /// result before the call that produced it.
    CatchingUp(Vec<Committed>),
    /// Caught up. Carries the highest seq folded, which is what makes the
    /// join with the queued facts exact — anything already in the history
    /// is skipped rather than folded twice.
    Live(SeqNo),
}

impl Facts {
    fn new(host: Arc<Host>) -> Self {
        Self {
            host,
            state: Mutex::new(Feed::CatchingUp(Vec::new())),
        }
    }

    /// Fold `c` now, or hold it for the catch-up. `true` when it reached the
    /// modules, which is the caller's cue that a frame is owed.
    ///
    /// The `seq` comparison is not belt-and-braces: the same fact can reach
    /// both paths, because reading the log and registering this listener are
    /// two steps and a commit can land between them. Whichever path folds it
    /// first wins; the other sees `seq <= high` and does nothing.
    fn deliver(&self, c: &Committed) -> bool {
        let mut state = self.state.lock().expect("fact feed poisoned");
        match &mut *state {
            Feed::CatchingUp(queue) => {
                queue.push(c.clone());
                false
            }
            Feed::Live(high) => {
                if c.seq <= *high {
                    return false;
                }
                *high = c.seq;
                self.host.absorb(&c.event);
                true
            }
        }
    }

    /// Fold the facts this process did not watch being committed, then let
    /// facts through as they arrive. `true` if anything was folded.
    ///
    /// The lock is held across the whole thing, which is what serialises it
    /// against `deliver` — so the modules see the history in log order and
    /// then the live facts in commit order, with no interleaving and no
    /// gap. The window is one startup, so holding it costs nothing.
    fn catch_up(&self, history: Vec<LoggedEvent>) -> bool {
        let mut state = self.state.lock().expect("fact feed poisoned");
        let queued = match std::mem::replace(&mut *state, Feed::Live(0)) {
            Feed::CatchingUp(queued) => queued,
            live @ Feed::Live(_) => {
                *state = live;
                return false;
            }
        };
        let mut high: SeqNo = 0;
        let mut folded = false;
        for (seq, event) in history
            .iter()
            .map(|l| (l.seq, &l.event))
            .chain(queued.iter().map(|c| (c.seq, &c.event)))
        {
            if seq <= high {
                continue;
            }
            high = seq;
            self.host.absorb(event);
            folded = true;
        }
        *state = Feed::Live(high);
        folded
    }
}

/// The assembled UI. Public so a test can drive exactly what ships.
pub struct Tui {
    /// The agent, driven through the handle protocol. Opened when the row
    /// mounts so the registry sees the agent before anyone can type; taken
    /// once, by `run`.
    driven: Mutex<Option<Driven>>,
    client: Mutex<Option<Arc<AgentClient>>>,
    host: Arc<Host>,
    keys: Keys,
    surface: Arc<dyn Surface>,
    /// Set when the loop starts. A command runs against the tree, and the tree
    /// is not known until then.
    ctx: Mutex<Option<Context>>,
    wake: Mutex<Option<mpsc::UnboundedSender<Wake>>>,
    /// Where the button went down, so a release can tell a click from a drag.
    pressed_at: Mutex<Option<(u16, u16)>>,
}

#[async_trait]
impl UserInterface for Tui {
    fn describe(&self) -> String {
        format!("full-screen terminal on {}", self.surface.describe())
    }

    async fn run(&self, ctx: &Context, initial: Option<String>) -> Result<(), String> {
        let Driven { handle, done, .. } = self
            .driven
            .lock()
            .expect("driven poisoned")
            .take()
            .ok_or("this front end can only be run once")?;
        let client = self
            .client
            .lock()
            .expect("client poisoned")
            .clone()
            .ok_or("no agent client; the row was not mounted")?;
        let AgentHandle {
            events: mut agent_events,
            ..
        } = handle;

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

        // What the agent says about itself — turn boundaries, a compaction's
        // outcome, an error — arrives over the handle. Content never does: the
        // transcript is a fold over the log (below), and these events only
        // move the status line and the command surface.
        let said = wake_tx.clone();
        let events_pump = tokio::spawn(async move {
            while let Some(event) = agent_events.recv().await {
                if said.send(Wake::Event(event)).is_err() {
                    break;
                }
            }
        });

        // Every committed fact reaches the modules here and nowhere else: the
        // screen is a fold over the log, so a resumed session and a live one
        // produce the same picture.
        //
        // One screen, one conversation — the same rule the handle's own pump
        // keeps. This listener sits at the root realm and a realm hears its
        // descendants, so every delegated member's facts arrive here too;
        // folding them in interleaves two conversations in one stream, which
        // is what a person sees as two agents talking over each other. What
        // the screen may show of a member is what the member *told* this
        // agent, and that is a fact in this log.
        let host = self.host.clone();
        let facts = wake_tx.clone();
        let mine = client.session().id().to_string();
        let ours = mine.clone();
        let feed = Arc::new(Facts::new(host));
        let subscribed = feed.clone();
        let stream = ctx.on_emit::<SessionEventCommitted>(move |c: &Committed| {
            if c.session != ours {
                return;
            }
            if !subscribed.deliver(c) {
                return;
            }
            let _ = facts.send(Wake::Fact);
        });

        // The history this process did not watch being committed.
        //
        // Registered before it is read, and folded through `Facts` rather than
        // straight from the log, so a fact committed in between is neither lost
        // (the listener has it) nor folded twice (the seq says who went first).
        // Without this a resumed session draws an empty conversation: a restored
        // log is put back silently, so nothing here ever heard about it.
        let history = client.session().events();
        if feed.catch_up(history) {
            let _ = wake_tx.send(Wake::Fact);
        }

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
            self.surface
                .set_motion(self.host.context_menu_open() || self.host.asks.is_waiting());
            if stale && (coalesced >= COALESCE_LIMIT || wake.is_empty()) {
                // The reading the frame about to be painted is drawn from. Facts
                // absorb against whatever the last one was — a frame at most out
                // of date, and the only reading available between commits.
                self.host.moment.write().expect("moment poisoned").now = clock.reading();
                self.refresh_members(ctx, &mine);
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
                    if !self.host.context_menu_open() && !self.host.asks.is_waiting() =>
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
        // The pump stops the turn and closes the asker in the one order that
        // does not deadlock, then says so. Wait for that rather than racing
        // it — but not forever: a tool that ignores its cancel is not a reason
        // to leave the terminal in the alternate screen.
        client.shutdown();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), done).await;
        events_pump.abort();
        self.host.asks.refuse_all();
        self.host.overlays.close_all();
        stream.dispose();
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

    /// Who else is running under this agent, as the registry has them now.
    ///
    /// Read fresh rather than folded, because there is nothing to fold: a
    /// member is created, opens a turn and is stopped without this
    /// conversation committing a single fact, and it must stay that way — a
    /// member's log is its own. Cheap by construction: the registry is a map
    /// read, and status and turn are one atomic load each, so this costs less
    /// than the repaint it precedes.
    fn refresh_members(&self, ctx: &Context, mine: &str) {
        use crate::moment::{Activity, MemberNow};
        use atomcode_harness::agent::AgentStatus;
        use atomcode_harness::seams::AgentsSvc;

        let Some(agents) = ctx.service::<AgentsSvc>() else {
            return;
        };
        let mut members: Vec<MemberNow> = agents
            .list()
            .iter()
            .filter(|a| a.parent() == Some(mine))
            .map(|a| MemberNow {
                name: a
                    .session_id()
                    .rsplit('/')
                    .next()
                    .unwrap_or(a.session_id())
                    .to_string(),
                activity: match a.status() {
                    AgentStatus::Idle => Activity::Idle,
                    AgentStatus::Working => Activity::Working,
                    AgentStatus::Stopping => Activity::Stopping,
                },
                turn: a.session().current_turn(),
            })
            .collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        let mut moment = self.host.moment.write().expect("moment poisoned");
        if moment.members != members {
            moment.members = members;
        }
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
                // conversation. The stream is bottom-anchored, so a block that
                // grows pushes its own header off the top — click a tool call
                // and the line you clicked is the first thing to leave. Moving
                // the view back by exactly what it gained keeps that line where
                // it was, and what appears, appears *below* it.
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
                    let name = match c.takes {
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
        for line in slot.block().content.lines(width.max(20)) {
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
/// The provider is the only thing that can answer: an adapter that cannot carry
/// image content degrades it to a plain-text caption, which is the right
/// compromise for a conversation being *resumed* on a text-only model and a
/// silent loss for a screenshot someone pasted a moment ago. Nothing in the
/// screen could tell those apart, which is why the question is asked here
/// instead of guessed from the model's name.
///
/// Asked of the *agent's own realm* — the same `LlmSvc` lookup the turn loop
/// makes in `drive_as` — so the answer cannot disagree with what happens to the
/// bytes on the wire. The empty slot is refused rather than waved through as a
/// default, even though a mounted tree cannot currently be in that state: the
/// `agent-loop` row depends on `llm`, so a tree without it does not start at
/// all. Refusing is the safe direction for a match arm that has to say
/// something, and it keeps the answer from being "sent" by omission if that
/// dependency ever loosens.
///
/// This runs when the picture is taken, not when the message is sent, so it
/// rests on one assumption: that the model cannot change between the two. In
/// this front end that holds — `/patch` is the only thing that re-points the
/// `llm` row, and it is a slash command, which cannot be run while a marker is
/// in the composer because the line would no longer start with `/`. A model
/// picker that swapped the row from a modal, leaving a draft intact underneath,
/// would break it; that is the change this comment is here to catch.
fn images_reach_the_model(client: &AgentClient) -> Result<(), String> {
    match client.agent.ctx().service::<LlmSvc>() {
        Some(provider) if provider.supports_vision() => Ok(()),
        Some(provider) => Err(format!(
            "当前模型 `{}` 看不了图片:贴进去也只会在发出去时被丢掉,所以没贴。\n\
             换成能看图的模型(改 `llm` 行)再贴。",
            provider.model_name()
        )),
        None => Err("这棵树里没有挂上模型(`llm` 行是空的),图片没有去处,所以没贴。".to_string()),
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
            driven: Mutex::new(None),
            client: Mutex::new(None),
            host,
            keys,
            surface,
            ctx: Mutex::new(None),
            wake: Mutex::new(None),
            pressed_at: Mutex::new(None),
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
        &["agents", "agent-loop", "surface"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // What the pump's projection reads, resolved live; where its own
        // agent's session comes from. **Not** who draws a question: a question
        // is a module riding the tail now, so this row neither asks for a
        // drawer nor depends on one being mounted.
        &["tools", "llm", "compaction", "session-defaults"]
    }
    fn provides(&self) -> &'static [&'static str] {
        // It owns the screen, so it is the one that can ask. The registries it
        // provides are filled by other rows — this row supplies the slots, not
        // the contents.
        &[
            "ui",
            "tui-agent-client",
            "tui-modules",
            "tui-commands",
            "tui-layout",
            "user-questions",
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
            .provide::<CommandsSvc>(host.commands.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<LayoutSvc>(host.layout.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<UserQuestionsSvc>(Arc::new(crate::ask::ScreenQuestions::new(
                host.asks.clone(),
            )))
            .map_err(|e| e.to_string())?;

        // The agent, behind the same pump every other driver uses. The screen
        // holds the questions, so it is what the pump releases on cancel.
        let wire = wire();
        let commands = wire.commands.clone();
        let driven = spawn_driver(ctx, wire, host.asks.clone(), CreateAgent::root(ctx)).await?;
        let client = Arc::new(AgentClient {
            commands,
            agent: driven.agent.clone(),
        });
        *tui.driven.lock().expect("driven poisoned") = Some(driven);
        *tui.client.lock().expect("client poisoned") = Some(client.clone());
        let _ = ctx
            .provide::<AgentClientSvc>(client)
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
        fn lines(&self, _w: u16) -> Vec<Line> {
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
