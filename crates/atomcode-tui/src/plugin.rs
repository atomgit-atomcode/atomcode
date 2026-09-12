//! The rows: how this UI mounts into a harness config tree.
//!
//! The whole UI is one `ui` row plus whatever module rows the layout names.
//! Nothing here is privileged — remove `tui-mascot` from the tree and the cat
//! is gone, with no branch left behind anywhere.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::events::SessionEventCommitted;
use atomcode_harness::agent::{Agent, CreateAgent};
use atomcode_harness::plugins::handle::{spawn as spawn_driver, wire, Driven};
use atomcode_harness::seams::{UiSvc, UserInterface, UserQuestionsSvc};
use atomcode_harness::session::Committed;
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_plexus::{plexus_service, Context, Plugin};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::host::{default_layout, Host};
use crate::keymap::{Action, Keys};
use crate::module::Modules;
use crate::surface::{Headless, Input, Surface, Terminal};

plexus_service!(SurfaceSvc => dyn Surface, "surface", Seam, "Where a frame is painted");
plexus_service!(ModulesSvc => Modules, "tui-modules", Core, "Mounted stream producers and view modules");
// The region tree, as a service, because more than one row needs it: the layout
// command set, the model's `adjust_layout` tool, and any panel that puts itself
// on screen when it mounts. Before it was a field on `Host` reachable only from
// inside this file, which is precisely why the mascot had to be a special case.
plexus_service!(LayoutSvc => crate::layout::Layout, "tui-layout", Core, "The region tree on screen");
plexus_service!(CommandsSvc => crate::command::Commands, "tui-commands", Core, "Slash commands contributed by rows");
plexus_service!(AgentClientSvc => AgentClient, "tui-agent-client", Core, "The command channel to the agent this screen drives");

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
    pub fn send(&self, text: String) {
        let _ = self.commands.send(AgentCommand::SendMessage {
            text,
            images: Vec::new(),
        });
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
        let stream = ctx.on_emit::<SessionEventCommitted>(move |c: &Committed| {
            if c.session != ours {
                return;
            }
            host.absorb(&c.event);
            let _ = facts.send(Wake::Fact);
        });

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
            client.send(text);
        }

        let mut quit = false;
        while !quit {
            self.refresh_members(ctx, &mine);
            self.paint();
            let timer = self.host.modules.tick();
            let woke = match timer {
                Some(every) => match tokio::time::timeout(every, wake.recv()).await {
                    Ok(Some(w)) => w,
                    Ok(None) => Wake::Closed,
                    Err(_) => Wake::Tick,
                },
                None => wake.recv().await.unwrap_or(Wake::Closed),
            };
            match woke {
                Wake::Closed => quit = true,
                Wake::Fact => {}
                Wake::Event(event) => self.on_event(event),
                Wake::Act(action) => quit = self.act(action, &client),
                Wake::Chose(chosen) => self.chose(chosen),
                Wake::Tick => {
                    let mut m = self.host.moment.write().expect("moment poisoned");
                    m.tick = m.tick.wrapping_add(1);
                }
                Wake::Input(Input::Resize(..)) => {}
                // The wheel scrolls the conversation, not the terminal's own
                // history — in the alternate screen that history is the shell's,
                // so a wheel the terminal keeps would scroll the wrong thing.
                Wake::Input(Input::Mouse(click, x, y)) => {
                    use crate::surface::Click;
                    // A press is not yet a click and not yet a selection —
                    // which it becomes is decided at the release, by whether
                    // the pointer moved. Deciding at the press would mean
                    // folding a block every time someone selects text on it.
                    let action = match click {
                        Click::WheelUp => Some(Action::Scroll(-WHEEL_LINES)),
                        Click::WheelDown => Some(Action::Scroll(WHEEL_LINES)),
                        Click::Press => {
                            *self.pressed_at.lock().expect("press poisoned") = Some((x, y));
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
                    };
                    if let Some(action) = action {
                        quit = self.act(action, &client);
                    }
                }
                Wake::Input(Input::Paste(text)) => {
                    quit = self.act(Action::Paste(text), &client);
                }
                // A modal has the keyboard while it is open, then the
                // question, then the ordinary bindings. Exactly one owner at a
                // time, decided here — that is what focus is.
                Wake::Input(Input::Key(press)) if self.host.overlays.is_open() => {
                    self.host.overlays.key(press);
                }
                // A question on screen gets first refusal on every key. Focus is
                // arbitration, not composition: exactly one thing can hold it,
                // and the host decides which.
                Wake::Input(Input::Key(press)) if self.host.asks.is_waiting() => {
                    quit = self.answer_question(press);
                }
                Wake::Input(Input::Key(press)) => {
                    if let Some(action) = self.keys.resolve(press) {
                        quit = self.act(action, &client);
                    }
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
        use atomcode_harness::agent::AgentStatus;
        use atomcode_harness::seams::AgentsSvc;
        use crate::moment::{Activity, MemberNow};

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
    fn on_event(&self, event: AgentEvent) {
        use crate::moment::Activity;
        match event {
            AgentEvent::TurnStarted => self.set_activity(Activity::Working),
            AgentEvent::TurnComplete { .. } | AgentEvent::Cancelled => {
                self.set_activity(Activity::Idle)
            }
            AgentEvent::Compacted { committed, .. } => {
                if committed {
                    self.say("已压缩");
                } else {
                    self.say("暂时没有值得压缩的");
                }
            }
            AgentEvent::Error { message, .. } => {
                self.set_activity(Activity::Idle);
                self.say_refused(&message);
            }
            _ => {}
        }
    }

    fn set_activity(&self, activity: crate::moment::Activity) {
        self.host.moment.write().expect("moment poisoned").activity = activity;
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
            Action::Insert(_) | Action::Backspace | Action::DeleteWord | Action::Paste(_)
        ) {
            m.history_at = None;
        }
        match action {
            Action::Quit => return true,
            Action::Submit => {
                let text = m.input.trim().to_string();
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
                client.send(text);
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
                let head = m.input[..caret].trim_end();
                let cut = head.rfind(' ').map(|i| i + 1).unwrap_or(0);
                m.input.replace_range(cut..caret, "");
                m.caret = cut;
            }
            Action::Clear => {
                m.input.clear();
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
            Action::Cancel => {
                m.activity = crate::moment::Activity::Stopping;
                drop(m);
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
                let size = self.surface.size();
                let before = self.host.stream_height(size.0);
                self.host
                    .presentation
                    .write()
                    .expect("presentation poisoned")
                    .toggle_block(id, kind);
                let grew = self.host.stream_height(size.0) as i64 - before as i64;
                let mut m = self.host.moment.write().expect("moment poisoned");
                let max = self.host.scroll_limit(size, &m) as i64;
                m.scroll =
                    crate::moment::ScrollPos((m.scroll.0 as i64 + grew).clamp(0, max) as usize);
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
                    "鼠标已收回:拖动选中并复制,点击工具调用折叠展开,滚轮滚动,esc 取消选中"
                        .to_string()
                } else {
                    "鼠标已交还终端:改用终端自己的框选(可跨 scrollback)。折叠用 ctrl-t/ctrl-r,滚动用 pgup/pgdn,ctrl-o 收回鼠标"
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
                m.activity = crate::moment::Activity::Stopping;
                drop(m);
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
            Action::ToggleModule(id) => {
                drop(m);
                self.toggle_module(id);
                return false;
            }
            Action::Layout(op) => {
                drop(m);
                // The same `apply` a command and the model's tool call. Three
                // ways in, one implementation.
                let known = known_modules(&self.host.modules);
                let said = match self.host.layout.apply(&op, &known) {
                    Ok(what) => (what, false),
                    Err(e) => (e.to_string(), true),
                };
                let mut stream = self.host.stream.write().expect("stream poisoned");
                let mut w = stream.writer("commands");
                w.emit(
                    crate::block::Coord::default(),
                    Arc::new(crate::content::CommandSaid {
                        text: said.0,
                        refused: said.1,
                    }),
                );
                return false;
            }
        }
        drop(m);
        // Every edit to the line can change what the menu should show.
        self.refresh_menu();
        false
    }

    /// Route one key to the question on screen. Returns `true` to quit.
    fn answer_question(&self, press: crate::surface::KeyPress) -> bool {
        use crate::surface::{Key, Mods};
        let Some(pending) = self.host.asks.peek() else {
            return false;
        };
        let (_, question, options) = pending;
        let chosen = match (press.key, press.mods) {
            // Esc and ctrl-c decline. Declining is an answer; it is never
            // consent, and it must always be one keystroke away.
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => Some(None),
            (Key::Char('d'), Mods::CTRL) => return true,
            (Key::Char(c), _) if c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap_or(0) as usize;
                options.get(n.wrapping_sub(1)).cloned().map(Some)
            }
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => options
                .iter()
                .find(|o| o.to_lowercase().starts_with(c.to_ascii_lowercase()))
                .cloned()
                .map(Some),
            // Enter takes the first option only when there is exactly one, so a
            // stray return can never approve a two-way choice.
            (Key::Enter, _) if options.len() == 1 => Some(options.first().cloned()),
            _ => None,
        };
        let Some(answer) = chosen else {
            return false; // an unrecognised key changes nothing
        };
        if let Some(p) = self.host.asks.take() {
            // Record what was asked and what was said, as a settled fact of the
            // conversation — the screen must be able to explain itself later.
            let mut stream = self.host.stream.write().expect("stream poisoned");
            let mut w = stream.writer("questions");
            w.emit(
                crate::block::Coord::default(),
                Arc::new(crate::content::ChoiceBlock {
                    question,
                    options,
                    answer: Some(answer.clone().unwrap_or_else(|| "declined".into())),
                }),
            );
            drop(stream);
            p.answer(answer);
        }
        false
    }

    /// Keep the slash menu in step with what is typed.
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
        if let Some(view) = self.host.modules.view(crate::modules::input::ID) {
            view.set_menu(menu);
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

    /// Toggle a panel's *visibility*, not its existence.
    ///
    /// The old version removed the view from the registry and could only put
    /// one back — the mascot — because constructing a panel is the row's job
    /// and this function had one type hard-coded. That is the wrong axis
    /// entirely: which panels exist is the tree's business, where they sit is
    /// the layout's, and a keystroke belongs to the second. So this is the
    /// same `Show`/`Hide` a slash command and the model produce.
    fn toggle_module(&self, id: &'static str) {
        let known: Vec<String> = self
            .host
            .modules
            .view_ids()
            .into_iter()
            .map(str::to_string)
            .collect();
        let showing = self.host.layout.tree().modules().iter().any(|n| n == id);
        let op = if showing {
            crate::layout::LayoutOp::Hide {
                module: id.to_string(),
            }
        } else {
            crate::layout::LayoutOp::Show {
                module: id.to_string(),
                side: crate::layout::Side::Top,
                size: None,
            }
        };
        let _ = self.host.layout.apply(&op, &known);
    }

    /// Print the conversation to the normal buffer on the way out.
    fn dump(&self) {
        use std::io::Write;
        let (w, _) = self.surface.size();
        let stream = self.host.stream.read().expect("stream poisoned");
        let mut out = String::new();
        for slot in stream.slots() {
            for line in slot.block().content.lines(w.max(20)) {
                out.push_str(&line.plain());
                out.push('\n');
            }
        }
        if !out.is_empty() {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
        }
    }
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

/// What a paste is allowed to put in the buffer.
///
/// Pasted text is arbitrary bytes from somewhere else: a log full of colour
/// escapes, a Windows file with CRLF, a table indented with tabs. Every span
/// this UI draws reaches the terminal verbatim, and the width arithmetic counts
/// a control character as zero cells — so an escape sequence in a paste is a
/// paste that can move the cursor, repaint the screen, or leave the alternate
/// screen entirely. It is stripped here, on the way in, rather than guarded
/// against at each of the places that later draw it.
///
/// Newlines survive, because they are content and the composer breaks on them.
/// Tabs become spaces: they are the other character whose drawn width is not
/// the width we counted.
fn sanitize_paste(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // A whole escape sequence, not just the ESC: dropping the ESC alone
            // would leave `[32m` sitting in the prompt as text.
            '\x1b' => match chars.next() {
                // CSI: parameters, then one final byte in @..~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: runs to BEL or to ST (`ESC \`).
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                            if c == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
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

/// Every module a layout may name — mounted or not.
fn known_modules(mods: &Modules) -> Vec<String> {
    mods.view_ids()
        .into_iter()
        .map(str::to_string)
        .chain(["mascot".to_string(), "findings".to_string()])
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

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
        // What the pump's projection reads, resolved live; and where its own
        // agent's session comes from.
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
        let driven =
            spawn_driver(ctx, wire, host.asks.clone(), CreateAgent::root(ctx)).await?;
        let client = Arc::new(AgentClient {
            commands,
            agent: driven.agent.clone(),
        });
        *tui.driven.lock().expect("driven poisoned") = Some(driven);
        *tui.client.lock().expect("client poisoned") = Some(client.clone());
        let _ = ctx
            .provide::<AgentClientSvc>(client)
            .map_err(|e| e.to_string())?;

        // The third way into the layout: the model. Perception is a prompt
        // fragment — read fresh on every request, so what the model believes
        // and what is on screen cannot drift — and action is a tool. Both are
        // provided by this one row and derived from the one layout, which is
        // why the description can never disagree with the picture.
        if let Some(prompts) = ctx.service::<atomcode_harness::seams::SystemPromptSvc>() {
            let l = host.layout.clone();
            let m = host.modules.clone();
            prompts.contribute("tui-layout", 60, l.describe_for_model(&known_modules(&m)));
            let id = "tui-layout";
            let p = prompts.clone();
            let _ = ctx.effect(move || p.remove(id));
        }
        if let Some(tools) = ctx.service::<atomcode_harness::seams::ToolsSvc>() {
            let tool: Arc<dyn atomcode_kernel::tool::Tool> =
                Arc::new(crate::layout_tool::AdjustLayout {
                    layout: host.layout.clone(),
                    modules: host.modules.clone(),
                });
            tools.register(tool).map_err(|e| e.to_string())?;
            let t = tools.clone();
            let _ = ctx.effect(move || t.unregister("adjust_layout"));
        }
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
mod paste_tests {
    use super::sanitize_paste;

    #[test]
    fn an_escape_sequence_in_a_paste_never_reaches_the_terminal() {
        // The hazard: spans are written verbatim and a control character is
        // counted as zero cells, so a pasted log could move the cursor, repaint
        // the screen, or leave the alternate screen.
        assert_eq!(sanitize_paste("\x1b[32mgreen\x1b[0m"), "green");
        assert_eq!(sanitize_paste("before\x1b[2Jafter"), "beforeafter");
        assert_eq!(sanitize_paste("\x1b]0;a title\x07x"), "x");
        assert_eq!(sanitize_paste("\x1b]11;rgb:00/00/00\x1b\\x"), "x");
        assert!(!sanitize_paste("\x1b[?1049lgone").contains('\x1b'));
    }

    #[test]
    fn newlines_survive_because_they_are_content() {
        // The composer breaks on them, and a paste that lost them would be a
        // paste that silently changed what the user is sending.
        assert_eq!(sanitize_paste("one\ntwo"), "one\ntwo");
        assert_eq!(
            sanitize_paste("crlf\r\nfile"),
            "crlf\nfile",
            "CRLF is one break"
        );
        assert_eq!(sanitize_paste("old\rmac"), "old\nmac");
    }

    #[test]
    fn a_tab_becomes_spaces_because_its_drawn_width_is_not_the_counted_one() {
        assert_eq!(sanitize_paste("a\tb"), "a    b");
    }

    #[test]
    fn ordinary_text_is_untouched_including_chinese_and_emoji() {
        for s in ["hello", "写一个网页", "🙂 ok", "path/to/file.rs:12"] {
            assert_eq!(sanitize_paste(s), s);
        }
    }
}
