//! The rows: how this UI mounts into a harness config tree.
//!
//! The whole UI is one `ui` row plus whatever module rows the layout names.
//! Nothing here is privileged — remove `tui-mascot` from the tree and the cat
//! is gone, with no branch left behind anywhere.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::agent::Agent;
use atomcode_harness::events::{AgentCreated, AgentInfo, SessionEventCommitted};
use atomcode_harness::seams::{AgentLoopSvc, AgentsSvc, UiSvc, UserInterface, UserQuestionsSvc};
use atomcode_harness::session::Committed;
use atomcode_plexus::{plexus_service, Context, Plugin};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::host::{default_layout, Host};
use crate::keymap::{Action, Keys};
use crate::module::{Modules, Mounted};
use crate::modules::{input, status, transcript};
use crate::surface::{Headless, Input, Surface, Terminal};

plexus_service!(SurfaceSvc => dyn Surface, "surface", Seam, "Where a frame is painted");
plexus_service!(ModulesSvc => Modules, "tui-modules", Core, "Mounted stream producers and view modules");
plexus_service!(CommandsSvc => crate::command::Commands, "tui-commands", Core, "Slash commands contributed by rows");

/// What woke the loop up.
enum Wake {
    Fact,
    Input(Input),
    /// An action from somewhere other than a key — a command, for now.
    Act(Action),
    /// A modal closed with this.
    Chose(Option<String>),
    Tick,
    Closed,
}

#[derive(Debug, Default, Deserialize)]
struct Row {
    /// Start with the mascot visible.
    #[serde(default)]
    mascot: bool,
}

/// The assembled UI. Public so a test can drive exactly what ships.
pub struct Tui {
    /// Whether a turn has been handed to the driver and not yet come back.
    ///
    /// `AgentStatus` cannot answer this: the driver sets `Working` inside the
    /// spawned task, so two lines submitted in quick succession both see `Idle`
    /// and both start a turn. The inbox is meant to fold the second into the
    /// first — this flag is what lets it.
    driving: Arc<std::sync::atomic::AtomicBool>,
    host: Arc<Host>,
    keys: Keys,
    surface: Arc<dyn Surface>,
    /// Set when the loop starts. A command runs against the tree, and the tree
    /// is not known until then.
    ctx: Mutex<Option<Context>>,
    wake: Mutex<Option<mpsc::UnboundedSender<Wake>>>,
}

#[async_trait]
impl UserInterface for Tui {
    fn describe(&self) -> String {
        format!("full-screen terminal on {}", self.surface.describe())
    }

    async fn run(&self, ctx: &Context, initial: Option<String>) -> Result<(), String> {
        let driver = ctx.require::<AgentLoopSvc>().map_err(|e| e.to_string())?;
        let agents = ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
        let agent = agents.create(ctx);
        ctx.emit::<AgentCreated>(&AgentInfo { id: agent.id() });

        let (wake_tx, mut wake) = mpsc::unbounded_channel::<Wake>();
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

        // Every committed fact reaches the modules here and nowhere else: the
        // screen is a fold over the log, so a resumed session and a live one
        // produce the same picture.
        let host = self.host.clone();
        let facts = wake_tx.clone();
        let stream = ctx.on_emit::<SessionEventCommitted>(move |c: &Committed| {
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
            agent.send(text);
            self.spawn_turn(&driver, &agent);
        }

        let mut quit = false;
        while !quit {
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
                Wake::Fact => self.sync_activity(&agent),
                Wake::Act(action) => quit = self.act(action, &agent, &driver),
                Wake::Chose(chosen) => self.chose(chosen),
                Wake::Tick => {
                    let mut m = self.host.moment.write().expect("moment poisoned");
                    m.tick = m.tick.wrapping_add(1);
                }
                Wake::Input(Input::Resize(..)) => {}
                Wake::Input(Input::Paste(text)) => {
                    quit = self.act(Action::Paste(text), &agent, &driver);
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
                        quit = self.act(action, &agent, &driver);
                    }
                }
            }
        }

        reader.abort();
        asks_pump.abort();
        // Release anything blocked on an answer that is never coming, or the
        // turn it belongs to would never end.
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

    fn sync_activity(&self, agent: &Arc<Agent>) {
        use atomcode_harness::agent::AgentStatus;
        let activity = match agent.status() {
            AgentStatus::Idle => crate::moment::Activity::Idle,
            AgentStatus::Working => crate::moment::Activity::Working,
            AgentStatus::Stopping => crate::moment::Activity::Stopping,
        };
        self.host.moment.write().expect("moment poisoned").activity = activity;
    }

    /// Start a turn unless one is already running. A message that arrives while
    /// one is in flight is claimed by that turn at its next step, which is what
    /// steering means.
    fn spawn_turn(&self, driver: &Arc<dyn atomcode_harness::seams::AgentLoop>, agent: &Arc<Agent>) {
        use std::sync::atomic::Ordering;
        if self
            .driving
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let driver = driver.clone();
        let agent = agent.clone();
        let flag = self.driving.clone();
        tokio::spawn(async move {
            driver.drive(&agent).await;
            flag.store(false, Ordering::SeqCst);
            // Anything that arrived after the loop decided it was done starts
            // the next turn rather than waiting for another keystroke.
            if agent.inbox().has_waking_input()
                && flag
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                driver.drive(&agent).await;
                flag.store(false, Ordering::SeqCst);
            }
        });
    }

    /// Apply one action. Returns `true` to quit.
    fn act(
        &self,
        action: Action,
        agent: &Arc<Agent>,
        driver: &Arc<dyn atomcode_harness::seams::AgentLoop>,
    ) -> bool {
        let mut m = self.host.moment.write().expect("moment poisoned");
        match action {
            Action::Quit => return true,
            Action::Submit => {
                let text = m.input.trim().to_string();
                m.input.clear();
                m.caret = 0;
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
                // Straight to the inbox: a line typed during a turn folds into
                // the turn already running rather than queueing.
                agent.send(text);
                self.spawn_turn(driver, agent);
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
            Action::Paste(text) => {
                let at = m.caret.min(m.input.len());
                m.input.insert_str(at, &text);
                m.caret = at + text.len();
            }
            Action::Cancel => {
                drop(m);
                agent.cancel();
                return false;
            }
            Action::Scroll(by) => {
                let width = self.surface.size().0;
                let max = self.host.stream_height(width);
                let next = m.scroll.0 as i64 - by as i64;
                m.scroll = crate::moment::ScrollPos(next.clamp(0, max as i64) as usize);
            }
            Action::ScrollToBottom => m.scroll = crate::moment::ScrollPos::BOTTOM,
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

    fn toggle_module(&self, id: &'static str) {
        let mods = &self.host.modules;
        if mods.has_view(id) {
            mods.remove_view(id);
        } else if id == "mascot" {
            let _ = mods.add_view(Arc::new(Mounted::<status::Mascot>::new()));
        }
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

/// Build the module registry a tree starts with.
pub fn base_modules(mascot: bool) -> Arc<Modules> {
    let mods = Arc::new(Modules::new());
    let _ = mods.add_producer(transcript::Transcript::new());
    let _ = mods.add_view(Arc::new(Mounted::<status::Status>::new()));
    let _ = mods.add_view(Arc::new(Mounted::<input::Input>::new()));
    if mascot {
        let _ = mods.add_view(Arc::new(Mounted::<status::Mascot>::new()));
    }
    mods
}

/// Assemble the whole UI over a surface. Shared by the row and by tests, so a
/// test drives exactly what ships.
pub fn assemble(surface: Arc<dyn Surface>, mascot: bool) -> (Arc<Host>, Tui) {
    let mods = base_modules(mascot);
    let layout = if mascot {
        use crate::region::{Constraint, Dir, Region};
        Region::split(
            Dir::Vertical,
            Constraint::Cells(1),
            Region::view("mascot"),
            default_layout(),
        )
    } else {
        default_layout()
    };
    let host = Arc::new(Host::new(mods, layout));
    let mut keys = Keys::new();
    keys.add(&crate::keymap::Default_).expect("default keys");
    (
        host.clone(),
        Tui {
            driving: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            host,
            keys,
            surface,
            ctx: Mutex::new(None),
            wake: Mutex::new(None),
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
        &["agents", "agent-loop", "sessions", "surface"]
    }
    fn provides(&self) -> &'static [&'static str] {
        // It owns the screen, so it is the one that can ask.
        &["ui", "tui-modules", "tui-commands", "user-questions"]
    }
    fn description(&self) -> &'static str {
        "a full-screen terminal UI assembled from module rows"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: Row = if config.is_null() {
            Row::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        // A hard dependency, declared in `inject`, so the surface row is always
        // mounted first. The terminal is a row like any other — not a fallback
        // baked in here, which is what made a headless tree quietly grab the
        // tty and fail on a machine with no terminal at all.
        let surface = ctx.require::<SurfaceSvc>().map_err(|e| e.to_string())?;
        let (host, tui) = assemble(surface, row.mascot);
        let _ = ctx
            .provide::<ModulesSvc>(host.modules.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<CommandsSvc>(host.commands.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<UserQuestionsSvc>(Arc::new(crate::ask::ScreenQuestions::new(
                host.asks.clone(),
            )))
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
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let term = Terminal::enter().map_err(|e| format!("cannot take the terminal: {e}"))?;
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
