//! The whole UI, driven end to end with no tty, no network, no model and no
//! human.
//!
//! Four sources of non-determinism, all removed by construction: the model is a
//! replay fixture, the terminal is a recorder, the keyboard is a script, and
//! `settle` is a quiescence predicate that **fails** on timeout rather than
//! passing. What is left is a test that either says something true or says
//! nothing at all.
//!
//! Two Apps, as they ship (`docs/adr/0022` §3): the agent's, mounted by the
//! harness's own host, and the screen's, mounted by `launch` — the same entry
//! the command line uses. They meet only over the connection the host hands
//! the screen.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_harness::seams::UserInterface;
use atomcode_harness::session::SessionEvent;
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin, PluginRegistry};
use atomcode_tree_host::{open, Opening, Registry, Trees};
use atomcode_tui::launch::{self, Screen};
use atomcode_tui::plugin::{AgentClientSvc, SurfaceSvc};
use atomcode_tui::surface::{Headless, Key, KeyPress, Surface};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// These frames are asserted in Chinese, for the reason the lib's own
/// `_tests_assert_in_chinese` gives. `tests/language.rs` holds the other half.
#[ctor::ctor]
fn _frames_are_asserted_in_chinese() {
    atomcode_tui::i18n::set_locale(atomcode_tui::i18n::Locale::ZhCn);
}

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("atui-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Holds the agent's driver between "the turn ended" being committed and the
/// agent being marked idle — the window a telemetry or trace subscriber occupies
/// in a real tree, widened so the screen reliably lands inside it.
struct HoldTurnEnd;

#[async_trait]
impl Plugin for HoldTurnEnd {
    fn name(&self) -> &'static str {
        "test-hold-turn-end"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        // `block_in_place`, not a bare sleep: a bare sleep pins this worker, and
        // the screen task the fact just woke sits in this worker's own run queue
        // until the hold ends — which hides the very race the hold exposes.
        let _ = ctx.on_emit::<atomcode_harness::events::TurnEnd>(
            |_: &atomcode_harness::seams::TurnOutcome| {
                tokio::task::block_in_place(|| std::thread::sleep(Duration::from_millis(300)));
            },
        );
        Ok(())
    }
}

/// Messages [`SlowPreStep`] has been handed, so a test knows when one is
/// being held.
static HELD_AT_PRE_STEP: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Holds a step's input in `PreStep` for two seconds when it says `SLOW-`:
/// the window between a turn claiming a message and the message entering the
/// log, which recognising a picture opens for real. A stop that lands in it
/// finds the message neither in the inbox nor in the conversation.
struct SlowPreStep;

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::PreStep> for SlowPreStep {
    async fn handle(
        &self,
        decision: &mut atomcode_harness::events::StepDecision,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::PreStep>,
    ) -> atomcode_harness::events::StepDecision {
        if let Some(message) = decision.message.clone().filter(|m| m.contains("SLOW-")) {
            HELD_AT_PRE_STEP
                .lock()
                .expect("held poisoned")
                .push(message);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        next.run(decision).await
    }
}

#[async_trait]
impl Plugin for SlowPreStep {
    fn name(&self) -> &'static str {
        "test-slow-pre-step"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let _ = ctx.on_waterfall::<atomcode_harness::events::PreStep>(Arc::new(SlowPreStep), false);
        Ok(())
    }
}

const SLOW_PRE_STEP: &str = "[[insert]]\nname = \"test-slow-pre-step\"\n";

/// Wait until the turn is holding `marker` in `PreStep`.
async fn held_at_pre_step(marker: &str) {
    for _ in 0..200 {
        if HELD_AT_PRE_STEP
            .lock()
            .expect("held poisoned")
            .iter()
            .any(|m| m.contains(marker))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{marker} never reached PreStep");
}

fn agent_catalog() -> PluginRegistry {
    let mut c = atomcode_harness::plugins::catalog();
    c.register(Arc::new(HoldTurnEnd));
    c.register(Arc::new(SlowPreStep));
    c.register(Arc::new(EffortSpyRow));
    c.register(Arc::new(EchoCommandRow));
    c.register(Arc::new(StallingUtilityRow));
    c.register(Arc::new(StallingModelRow));
    c.register(Arc::new(RequestUserInputRow));
    c
}

/// Both halves of what a test runs: the agent's layers and the screen's.
struct Setup {
    agent: Vec<String>,
    screen: Vec<String>,
}

/// A layer about the screen rather than the agent. The tests hand extra layers
/// to one list; they are sorted by what they name.
fn is_screen_layer(layer: &str) -> bool {
    layer.contains("\"surface\"") || layer.contains("\"tui-")
}

fn agent_base(root: &Path, persistence: &str, session: &str) -> String {
    let empty = root.join("__no_skills__");
    let _ = std::fs::create_dir_all(&empty);
    format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"mcp\"\ndisabled = true\n\n\
         [[patch]]\nid = \"tool-web\"\ndisabled = true\n\n\
         {persistence}\n\
         [[patch]]\nid = \"approval\"\ndisabled = false\nconfig = {{ mode = \"yolo\" }}\n\n\
         [[patch]]\nid = \"approval-interactive\"\ndisabled = true\n\n\
         [[patch]]\nid = \"user-questions-unattended\"\ndisabled = true\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 8, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         {session}\
         [[patch]]\nid = \"ui\"\nname = \"ui-handle-questions\"\nconfig = {{ ask_timeout_secs = 0 }}\n",
        root = root.to_string_lossy(),
        home = empty.to_string_lossy()
    )
}

fn setup(base: String, script: &str, extra: &[&str]) -> Setup {
    let mut agent = vec![
        atomcode_harness::bundle::ONESHOT_APP.to_string(),
        base,
        script.to_string(),
    ];
    let mut screen = Vec::new();
    for layer in extra {
        if is_screen_layer(layer) {
            screen.push(layer.to_string());
        } else {
            agent.push(layer.to_string());
        }
    }
    Setup { agent, screen }
}

/// A tree with the model scripted, the world pinned to `root`, and the screen
/// painted into memory.
fn tree(root: &Path, script: &str, extra: &[&str]) -> Setup {
    let persistence = "[[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n";
    setup(agent_base(root, persistence, ""), script, extra)
}

fn replay(steps: &str) -> String {
    format!("[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {steps} ] }}\n")
}

/// The scripted model, plus the reasoning-effort levels it claims to expose —
/// so the `/effort` menu has levels to offer. The screen reads what a model
/// supports off its description (`effort_levels`, a model capability like
/// `supports_vision`); a model that declares none collapses the menu to
/// `default` alone, on purpose.
fn replay_effort(steps: &str, levels: &[&str]) -> String {
    let list = levels
        .iter()
        .map(|l| format!("\"{l}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = \
         {{ script = [ {steps} ], effort_levels = [ {list} ] }}\n"
    )
}

/// Every reasoning-effort level, for a test that wants the full menu.
const ALL_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// The same screen as [`tree`], but with a session that persists and can be
/// resumed — pointed at a private `home` so one test cannot see (or be seen by)
/// any session on the machine.
fn tree_resumable(
    root: &Path,
    home: &Path,
    id: &str,
    resume: bool,
    script: &str,
    extra: &[&str],
) -> Setup {
    let sessions = home.join("sessions");
    let _ = std::fs::create_dir_all(&sessions);
    let persistence = format!(
        "[[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {sessions:?} }}\n"
    );
    let session =
        format!("[[patch]]\nid = \"session\"\nconfig = {{ id = {id:?}, resume = {resume} }}\n\n");
    setup(agent_base(root, &persistence, &session), script, extra)
}

/// Every fact of a session that has reached its file under `home` so far.
fn persisted_facts(home: &Path, id: &str) -> Vec<SessionEvent> {
    fn find(dir: &Path, name: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find(&path, name) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|n| n == name) {
                return Some(path);
            }
        }
        None
    }
    let Some(file) = find(&home.join("sessions"), &format!("{id}.jsonl")) else {
        return Vec::new();
    };
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            serde_json::from_value::<SessionEvent>(value.get("event")?.clone()).ok()
        })
        .collect()
}

/// Wait for the fire-and-forget persistence writer to land `want` facts.
///
/// The writer is deliberately off the turn's path (a queue behind one task), so
/// "the turn finished" and "the file has it" are different moments. A resume
/// test that skipped this would be racing its own fixture.
async fn persisted(home: &Path, id: &str, want: usize) {
    for _ in 0..100 {
        if persisted_facts(home, id).len() >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the log never reached {want} facts on disk");
}

/// The same scripted model, plus an answer to "can you see pictures?".
fn replay_vision(steps: &str, vision: bool) -> String {
    format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = \
         {{ supports_vision = {vision}, script = [ {steps} ] }}\n"
    )
}

/// Every image that reached the conversation, in order. The log is the only
/// place that can say whether an attachment was actually *sent* — the marker on
/// screen says what was typed, not what the model received.
fn images_sent(s: &Session) -> Vec<usize> {
    s.client()
        .events()
        .into_iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::UserMessage { images, .. } => Some(images.len()),
            _ => None,
        })
        .collect()
}

struct Session {
    /// Held, not just borrowed from: dropping the screen's `App` unloads its
    /// tree, and the agent's goes with the connection it holds.
    _app: Arc<tokio::sync::Mutex<App>>,
    app: Context,
    term: Arc<Headless>,
    ui: Arc<dyn UserInterface>,
}

async fn start(setup: Setup) -> Session {
    start_with_host(setup, |control| control).await
}

/// The same, with the host's control wrapped on the way to the screen.
///
/// For the questions the screen asks the host that no fixture answers by
/// itself — readiness, for one. The wrapper sits where the real host's does, so
/// what is under test is the screen's half of the exchange.
async fn start_with_host(
    setup: Setup,
    wrap: impl FnOnce(
        Arc<dyn atomcode_host_api::HostControl>,
    ) -> Arc<dyn atomcode_host_api::HostControl>,
) -> Session {
    start_with_connection(setup, move |connection| {
        let atomcode_host_api::HostConnection {
            session,
            commands,
            events,
            control,
        } = connection;
        atomcode_host_api::HostConnection {
            session,
            commands,
            events,
            control: wrap(control),
        }
    })
    .await
}

/// The same, plus a way to say something to the screen as if the agent had.
///
/// The connection's event stream is a channel, so a test can sit in it: what
/// the agent sends still arrives, and the test can add to it. For the events a
/// fixture cannot produce on demand — a compaction that failed to write its
/// checkpoint, a provider giving up — where what is worth judging is what the
/// screen does about it rather than how it came to happen.
async fn start_with_agent_events(
    setup: Setup,
) -> (
    Session,
    tokio::sync::mpsc::UnboundedSender<atomcode_kernel::event::AgentEvent>,
) {
    let carried: Arc<std::sync::Mutex<Option<_>>> = Arc::new(std::sync::Mutex::new(None));
    let slot = carried.clone();
    let session = start_with_connection(setup, move |connection| {
        let atomcode_host_api::HostConnection {
            session,
            commands,
            mut events,
            control,
        } = connection;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *slot.lock().expect("injector poisoned") = Some(tx.clone());
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        atomcode_host_api::HostConnection {
            session,
            commands,
            events: rx,
            control,
        }
    })
    .await;
    let tx = carried
        .lock()
        .expect("injector poisoned")
        .clone()
        .expect("the wrapper ran");
    (session, tx)
}

/// The screen, with the connection the host hands it put through `wrap` first.
async fn start_with_connection(
    setup: Setup,
    wrap: impl FnOnce(atomcode_host_api::HostConnection) -> atomcode_host_api::HostConnection,
) -> Session {
    start_full(setup, wrap, Ports::default()).await
}

/// A settings port that answers one row with `value` and knows nothing else.
///
/// A stand-in for the launcher's, and deliberately a *port* rather than a
/// fixture written into the moment: the whole question of the light is whether
/// the screen reads the configuration it is handed, so the test has to hand it
/// one the way the product does.
struct OneSetting(&'static str, &'static str);

impl atomcode_tui::settings::Settings for OneSetting {
    fn rows(&self) -> atomcode_tui::settings::SettingsView {
        atomcode_tui::settings::SettingsView::new(vec![atomcode_tui::settings::SettingRow {
            id: self.0.to_string(),
            label: self.0.to_string(),
            value: self.1.to_string(),
            kind: atomcode_tui::settings::SettingKind::Boolean,
            applies: atomcode_tui::settings::Applies::Immediately,
        }])
    }
    fn set(&self, _id: &str, _value: &str) -> Result<atomcode_tui::settings::SettingsView, String> {
        Err("this port cannot write".into())
    }
    fn reset(&self, _id: &str) -> Result<atomcode_tui::settings::SettingsView, String> {
        Err("this port cannot write".into())
    }
}

async fn start_with_connection_and_settings(
    setup: Setup,
    settings: Option<Arc<dyn atomcode_tui::settings::Settings>>,
) -> Session {
    start_full(
        setup,
        |control| control,
        Ports {
            settings,
            ..Default::default()
        },
    )
    .await
}

/// The layer that puts the test's plugins panel on screen.
const PLUGINS_PANEL_LAYER: &str = "[[insert]]\nname = \"tui-panel-plugins\"\n";

/// The plugins panel's view, mounted the way the launcher's row mounts it.
struct PluginsPanelRow;

#[async_trait]
impl Plugin for PluginsPanelRow {
    fn name(&self) -> &'static str {
        "tui-panel-plugins"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let mods = ctx
            .require::<atomcode_tui::plugin::ModulesSvc>()
            .map_err(|e| e.to_string())?;
        mods.add_view(Arc::new(atomcode_tui::module::Mounted::<
            atomcode_tui::modules::plugins::Plugins,
        >::new()))?;
        Ok(())
    }
}

/// Start the screen with a plugins port behind it, which is the only way to
/// see the panel at all: a screen with no port refuses to open it.
async fn start_with_plugins(
    setup: Setup,
    plugins: Arc<dyn atomcode_tui::plugins::Plugins>,
) -> Session {
    start_full(
        setup,
        |control| control,
        Ports {
            plugins: Some(plugins),
            ..Default::default()
        },
    )
    .await
}

/// 一次开屏要填的那几条缝。
///
/// 收成一个结构体而不是排成一长串参数:它们全是 `Option`、全是
/// `Arc<dyn _>`,排成参数的话相邻两个写反了编译器一句话不说。
/// 和 `launch::Ports` 同一个形状,也是同一个理由。
#[derive(Default)]
struct Ports {
    settings: Option<Arc<dyn atomcode_tui::settings::Settings>>,
    plugins: Option<Arc<dyn atomcode_tui::plugins::Plugins>>,
    tools: Option<Arc<dyn atomcode_tui::tools::Tools>>,
    rewind: Option<Arc<dyn atomcode_tui::rewind::Rewind>>,
    resume: Option<Arc<dyn atomcode_tui::resume::Resume>>,
    shell: Option<Arc<dyn atomcode_tui::shell::Shell>>,
    remote: Option<Arc<dyn atomcode_tui::remote::Remote>>,
}

async fn start_full(
    setup: Setup,
    wrap: impl FnOnce(atomcode_host_api::HostConnection) -> atomcode_host_api::HostConnection,
    ports: Ports,
) -> Session {
    let Ports {
        settings,
        plugins,
        tools,
        rewind,
        resume,
        shell,
        remote,
    } = ports;
    let agent_layers = setup.agent.clone();
    let registry: Registry = Arc::new(agent_catalog);
    let trees: Trees = Arc::new(move |opening: &Opening| {
        let mut layers = vec![atomcode_harness::bundle::base().map_err(|e| e.to_string())?];
        let mut texts = agent_layers.clone();
        if let Opening::Resume(id) = opening {
            texts.push(atomcode_harness::bundle::resume_overlay(id));
        }
        for text in &texts {
            layers.push(Layer::from_toml(text).map_err(|e| e.to_string())?);
        }
        ConfigTree::from_layers(layers).map_err(|e| e.to_string())
    });
    let connection = open(registry, trees, Opening::Fresh)
        .await
        .expect("the agent's tree must mount");
    let connection = wrap(connection);
    let screen = Screen {
        headless: Some((80, 24)),
        ..Screen::default()
    };
    let mut extra: Vec<&str> = setup.screen.iter().map(String::as_str).collect();
    // The panel's *view* is the launcher's row in the shipped product
    // (`atomcode::tui_plugins`), so a test that wants the panel on screen brings
    // one of its own — the same bargain the settings and providers panels
    // strike, and the reason a screen with no launcher opens without them.
    let mut panel_row: Vec<Arc<dyn Plugin>> = match plugins.is_some() {
        true => {
            extra.push(PLUGINS_PANEL_LAYER);
            vec![Arc::new(PluginsPanelRow)]
        }
        false => Vec::new(),
    };
    // Same bargain for the tools panel, except that its row carries the port
    // too — in the shipped product `atomcode::tui_tools` provides
    // `ToolCatalogSvc` from the same row that mounts the view.
    if let Some(tools) = tools {
        extra.push(TOOLS_PANEL_LAYER);
        panel_row.push(Arc::new(ToolsPanelRow(tools)));
    }
    // And for the rewind panel, whose row carries its port too
    // (`atomcode::tui_rewind`).
    if let Some(rewind) = rewind {
        extra.push(REWIND_PANEL_LAYER);
        panel_row.push(Arc::new(RewindPanelRow(rewind)));
    }
    // And for the resume panel (`atomcode::tui_resume`). Its list arrives with
    // the `/resume` command's own round trip; the port is only for throwing a
    // stored session away, which the panel starts by itself.
    if let Some(resume) = resume {
        extra.push(RESUME_PANEL_LAYER);
        panel_row.push(Arc::new(ResumePanelRow(resume)));
    }
    // 本机 shell 同理(`atomcode::tui_shell`):填了这条缝,`!` 才算数。
    if let Some(shell) = shell {
        extra.push(SHELL_ROW_LAYER);
        panel_row.push(Arc::new(ShellPanelRow(shell)));
    }
    // 远端那条路同理(`atomcode::tui_share`):没填就没有远端。
    if let Some(remote) = remote {
        extra.push(REMOTE_ROW_LAYER);
        panel_row.push(Arc::new(RemotePanelRow(remote)));
    }
    let mounted = launch::mount_with(
        &screen,
        &extra,
        &panel_row,
        launch::Ports {
            settings,
            providers: None,
            plugins,
            setup: None,
        },
        connection,
    )
    .await
    .expect("the screen's tree must mount");
    let surface = mounted
        .app
        .context()
        .service::<SurfaceSvc>()
        .expect("the headless surface row provides `surface`");
    let term = surface
        .as_any_headless()
        .expect("this tree mounts the headless surface");
    let ctx = mounted.app.context();
    Session {
        _app: Arc::new(tokio::sync::Mutex::new(mounted.app)),
        app: ctx,
        term,
        ui: mounted.ui,
    }
}

impl Session {
    fn client(&self) -> Arc<atomcode_tui::plugin::AgentClient> {
        self.app
            .service::<AgentClientSvc>()
            .expect("the screen provides its client")
    }

    /// Run the UI in the background and wait for the first frame.
    async fn open(&self) -> tokio::task::JoinHandle<()> {
        let ui = self.ui.clone();
        let ctx = self.app.clone();
        let handle = tokio::spawn(async move {
            let _ = ui.run(&ctx, None).await;
        });
        assert!(
            self.term
                .settle(Duration::from_millis(40), Duration::from_secs(5))
                .await,
            "the UI never painted a first frame"
        );
        handle
    }

    /// Wait until nothing is happening.
    ///
    /// Screen quiescence alone is not enough: while a tool runs for half a
    /// second no frame changes, and a test that took that for "done" would
    /// assert on a half-finished turn. The predicate is both — frames stopped
    /// **and** the agent says it is settled — and it fails on timeout rather
    /// than passing, because a `settle` that gives up quietly is a test that
    /// passes while nothing happened.
    async fn quiet(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            // Ask what the agent said, not what is drawn: reading "idle" off the
            // status bar worked until a test hid the status bar. Settle *first*,
            // then ask: checking busy before waiting lets a turn start during the
            // wait and still be reported quiet. Settled means nothing sent is
            // still waiting for a turn to take it, and the agent is idle.
            let still = self
                .term
                .settle(Duration::from_millis(60), Duration::from_secs(5))
                .await;
            if still && self.client().settled() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "never went quiet within 30s; last frame:\n{}",
            self.term.text()
        );
    }

    fn screen(&self) -> String {
        self.term.text()
    }
}

// ---- the flow a person actually performs --------------------------------

#[tokio::test]
async fn a_new_session_opens_with_the_welcome_and_it_then_scrolls_away() {
    let dir = scratch("welcome");
    // One short turn, so the welcome has something to be pushed out by.
    let script = replay(r#"{ text = "Ready." }"#);
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    // 1. It is there at the start, at the top of the conversation.
    let opening = s.screen();
    assert!(opening.contains("AtomCode"), "the brand row:\n{opening}");
    assert!(opening.contains("上手提示"), "the tips heading:\n{opening}");
    // Where we are, as the block writes it: the same folding the welcome does, so
    // the assertion does not depend on where the scratch directory happens to be.
    let here =
        atomcode_tui::text::collapse_home(&std::env::current_dir().unwrap().to_string_lossy());
    let here = here.split('/').next_back().unwrap_or("");
    assert!(
        opening.contains(here) || opening.contains("~/"),
        "where we are (expected something like `{here}`):\n{opening}"
    );

    // **And it is in the first row of the conversation, not the last.** "At the
    // top of the conversation" was true of the block's *order* while the screen
    // said otherwise: short content was pushed to the foot of the pane, so the
    // opening sat against the composer with the blank rows above it, and a new
    // session looked like a screen that had ended. Anchored to the empty
    // conversation's own first row rather than to the pane's, so a layout with
    // something above the conversation is not what this measures.
    let stream = s
        .term
        .last()
        .expect("a frame")
        .part("stream")
        .expect("the conversation")
        .rect;
    let first = opening
        .lines()
        .position(|l| !l.trim().is_empty())
        .expect("the opening is drawn");
    assert_eq!(
        first as u16, stream.y,
        "the opening starts where the conversation does, not at its foot:\n{opening}"
    );

    // 2. A turn happens, which pushes it off the top.
    s.term.type_line("hello");
    s.quiet().await;
    let after = s.screen();
    assert!(after.contains("Ready."), "the answer:\n{after}");

    // 3. Scrolling back up finds it again — it is stream content, not a panel
    //    pinned on screen. This is the property the whole shape was chosen for:
    //    a view module would have been cheaper but could not do this.
    for _ in 0..8 {
        s.term
            .pointer(atomcode_tui::surface::Click::WheelUp, 10, 10);
    }
    s.quiet().await;
    let scrolled = s.screen();
    assert!(
        scrolled.contains("AtomCode"),
        "scrolling back up must find the welcome again:\n{scrolled}"
    );

    task.abort();
}

/// Two Ctrl+C on an idle empty line exits — the reflex people reach for. A single
/// one only arms it (and hints), so a stray press never quits from under you.
#[tokio::test]
async fn two_ctrl_c_on_an_idle_line_exits() {
    let dir = scratch("ctrlc-quit");
    let s = start(tree(&dir, &replay(r#"{ text = "hi" }"#), &[])).await;
    let task = s.open().await;

    // First Ctrl+C arms and hints — it must NOT exit.
    s.term.press(KeyPress::ctrl('c'));
    s.term
        .settle(Duration::from_millis(60), Duration::from_secs(5))
        .await;
    assert!(!task.is_finished(), "one Ctrl+C must not quit");
    assert!(
        s.screen().contains("再按 Ctrl+C 退出"),
        "the first press shows the confirm hint:\n{}",
        s.screen()
    );

    // Second Ctrl+C quits: the run loop returns, so awaiting the task completes.
    s.term.press(KeyPress::ctrl('c'));
    let quit = tokio::time::timeout(Duration::from_secs(5), task).await;
    assert!(quit.is_ok(), "two Ctrl+C must exit");
}

/// A single Ctrl+C is disarmed by any other key, so it takes a fresh pair to
/// quit — a stray press followed by real typing never leaves the terminal one
/// keystroke from exit.
#[tokio::test]
async fn a_ctrl_c_is_disarmed_by_other_input() {
    let dir = scratch("ctrlc-disarm");
    let s = start(tree(&dir, &replay(r#"{ text = "hi" }"#), &[])).await;
    let task = s.open().await;

    s.term.press(KeyPress::ctrl('c')); // arm
    s.term.type_text("x"); // any other input disarms
    s.term
        .settle(Duration::from_millis(60), Duration::from_secs(5))
        .await;
    // This Ctrl+C is a fresh first press (re-arms), not a quit.
    s.term.press(KeyPress::ctrl('c'));
    s.term
        .settle(Duration::from_millis(60), Duration::from_secs(5))
        .await;
    assert!(
        !task.is_finished(),
        "a Ctrl+C after other input must re-arm, not quit"
    );

    task.abort();
}

#[tokio::test]
async fn a_resumed_session_opens_with_a_welcome_above_its_history() {
    // The welcome rides the top of a resumed conversation, not only a fresh one.
    // The stream is emission-ordered, so it sits on top by being emitted first:
    // the loop opens the moment the session is described — before the backfill
    // folds in — and the history then lands beneath it.
    let home = scratch("welcome-resume-home");
    let root = scratch("welcome-resume-work");
    let id = "welcomed-once";

    {
        let s = start(tree_resumable(
            &root,
            &home,
            id,
            false,
            &replay(r#"{ text = "It is 42." }"#),
            &[],
        ))
        .await;
        let task = s.open().await;
        s.term.type_line("remember the number 42");
        s.quiet().await;
        persisted(&home, id, 6).await;
        s.term.press(KeyPress::ctrl('d'));
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    let s = start(tree_resumable(
        &root,
        &home,
        id,
        true,
        &replay(r#"{ text = "still 42." }"#),
        &[],
    ))
    .await;
    let task = s.open().await;
    let screen = s.screen();
    assert!(
        screen.contains("remember the number 42"),
        "the resumed screen shows the history:\n{screen}"
    );

    // **A resumed session opens with the welcome, once, at the top.**
    //
    // The block is still deliberately **not** a logged fact (`open_conversation`
    // writes the stream directly, so a resume does not replay it and persistence
    // does not record it as something the session said). So a resumed
    // conversation folds a log that never had it, and this re-emits it on top per
    // view — exactly once, above the history.
    for _ in 0..40 {
        s.term
            .pointer(atomcode_tui::surface::Click::WheelUp, 10, 10);
    }
    s.quiet().await;
    let top = s.screen();
    assert_eq!(
        top.matches("上手提示").count(),
        1,
        "a resumed session opens with the welcome, and only once:\n{top}"
    );
    let heading = top.find("上手提示").expect("the welcome heading");
    let history = top
        .find("remember the number 42")
        .expect("the history is still all there");
    assert!(
        heading < history,
        "the welcome sits above the history it opened:\n{top}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A session started with `/clear` opens with the welcome too.
///
/// The block is produced by `open_conversation` and by nothing else, and the
/// loop asks that question through a flag that is *lowered* once it has an
/// answer — on the reasoning that a stream which was not empty will not become
/// empty again. `switch_session` is exactly the thing that makes it empty
/// again (`host.rs` `switch_view`), and it used to leave the flag down: a new
/// session opened bare, with no welcome and no cat, while every other criterion
/// stayed green because they all start a session rather than *move to* one.
#[tokio::test]
async fn a_new_session_started_from_the_screen_opens_with_the_welcome_too() {
    let home = scratch("welcome-switch-home");
    let root = scratch("welcome-switch-work");
    let s = start(tree_persistent(&root, &home, &replay(r#"{ text = "ok" }"#))).await;
    let task = s.open().await;

    // The first session opens with it — the property that already held.
    s.quiet().await;
    assert!(
        s.screen().contains("上手提示"),
        "the session it started with:\n{}",
        s.screen()
    );

    let first = s.client().session();
    s.term.type_line("/clear");
    moved_from(&s, &first).await;
    s.quiet().await;

    let fresh = s.screen();
    assert!(
        fresh.contains("已切换到会话"),
        "the switch happened:\n{fresh}"
    );
    // The same two things the first session showed: the tips heading, and the
    // cat's art. The cat is not decoration here — it is the reason this
    // criterion looks at the glyph rather than only at the heading, since the
    // block's *text* would survive a mascot that stopped being drawn.
    assert!(
        fresh.contains("上手提示"),
        "the session it moved to owes its own first word:\n{fresh}"
    );
    assert!(
        fresh.contains('\u{2580}'),
        "and the cat came with it — the welcome draws no half-block without it:\n{fresh}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_person_types_a_question_and_reads_the_answer() {
    let dir = scratch("basic");
    std::fs::write(dir.join("a.rs"), "fn main() {}").unwrap();
    let script = replay(
        r#"{ text = "Reading it.", calls = [ { name = "read_file", args = { file_path = "a.rs" } } ] },
           { text = "It is an empty main." }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("what is in a.rs?");
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("what is in a.rs?"),
        "the question:\n{screen}"
    );
    assert!(screen.contains("ReadFile"), "the tool it used:\n{screen}");
    assert!(
        screen.contains("It is an empty main"),
        "the answer:\n{screen}"
    );
    assert!(
        s.term.last().unwrap().part("status").is_some(),
        "the status line is on screen"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// ---- a picture the model cannot receive is never sent quietly -----------

fn screenshot(tag: &str) -> atomcode_kernel::message::ImageContent {
    atomcode_kernel::message::ImageContent {
        media_type: "image/png".into(),
        data: tag.to_string(),
    }
}

#[tokio::test]
async fn a_picture_pasted_for_a_text_model_attaches_for_the_runtime_to_handle() {
    // The paste is no longer refused at the keystroke. A text-only model has the
    // runtime's VL preprocessor caption the image — a configured, or a
    // `/codingplan`-auto-detected "default vision", helper — or, failing that,
    // fold a marker and clear the bytes on send; the "never sent quietly"
    // guarantee moved from here to the turn (see `atomcode-cli`'s
    // `VlImagePreprocessor`). This harness mounts no preprocessor (it is the
    // harness's own host, not the coding runtime), so it checks only what the
    // screen does at paste time: the picture attaches, and nothing is refused.
    let dir = scratch("blind-paste");
    let s = start(tree(&dir, &replay_vision(r#"{ text = "ok" }"#, false), &[])).await;
    let task = s.open().await;
    s.term.set_clipboard_image(screenshot("blind"));

    s.term.press(KeyPress::ctrl('v'));
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("[Image #1]"),
        "the picture attaches; the runtime decides its fate on send:\n{screen}"
    );
    assert!(
        !screen.contains("看不了图片"),
        "a text-only model is no longer refused at paste time:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_picture_pasted_for_a_model_that_can_see_it_is_attached_and_sent() {
    // The other direction, which is what keeps the gate a gate rather than a
    // wall: a vision model still gets the picture.
    let dir = scratch("vision-paste");
    let s = start(tree(&dir, &replay_vision(r#"{ text = "ok" }"#, true), &[])).await;
    let task = s.open().await;
    s.term.set_clipboard_image(screenshot("vision"));

    s.term.press(KeyPress::ctrl('v'));
    s.quiet().await;
    assert!(
        s.screen().contains("[Image #1]"),
        "the marker is in what is being typed:\n{}",
        s.screen()
    );

    s.term.type_line("看这个");
    s.quiet().await;
    assert_eq!(
        images_sent(&s),
        vec![1],
        "the image was in the message the model received"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn clearing_the_composer_takes_the_picture_with_it() {
    // Ctrl+U throws the text away, and `[Image #1]` was part of that text.
    //
    // This locks what a person can see: the marker goes with the text, and the
    // turn that follows carries no image. It does NOT witness the composer
    // releasing the bytes — that is deliberately unobservable from here,
    // because a marker number is never reused (`add` only ever moves `next`
    // forward), so an image left held after a clear can never be referred to
    // again and `take_shown` filters it out of every later send. The release is
    // therefore a state-coherence and memory bound, and its witness is the unit
    // test in `attach.rs`, not this one.
    let dir = scratch("clear-attachment");
    let s = start(tree(&dir, &replay_vision(r#"{ text = "ok" }"#, true), &[])).await;
    let task = s.open().await;
    s.term.set_clipboard_image(screenshot("cleared"));

    s.term.press(KeyPress::ctrl('v'));
    s.quiet().await;
    assert!(
        s.screen().contains("[Image #1]"),
        "the picture is attached to start with:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('u'));
    s.quiet().await;
    assert!(
        !s.screen().contains("[Image #1]"),
        "the marker is gone with the text:\n{}",
        s.screen()
    );

    s.term.type_line("清空之后只发文字");
    s.quiet().await;
    assert_eq!(
        images_sent(&s),
        vec![0],
        "the turn after a clear carries text, not the cleared picture"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn every_frame_respects_every_rect() {
    let dir = scratch("containment");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.term.type_line("hello 中文 🙂");
    s.quiet().await;

    // The pixel-level verdict on spatial composability, over the whole run
    // rather than one frame.
    for (i, frame) in s.term.frames().iter().enumerate() {
        assert!(
            frame.containment_violations().is_empty(),
            "frame {i}: {:?}",
            frame.containment_violations()
        );
        assert_eq!(frame.rows().len(), 24, "frame {i} is the wrong height");
        for row in frame.rows() {
            assert!(
                atomcode_tui::width::str_width(&row) <= 80,
                "frame {i} row overflows: {row:?}"
            );
        }
    }
    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn typing_during_a_turn_is_folded_into_it_rather_than_queued() {
    let dir = scratch("steer");
    std::fs::write(dir.join("a.rs"), "x").unwrap();
    let script = replay(
        r#"{ text = "Working.", calls = [ { name = "read_file", args = { file_path = "a.rs" } } ] },
           { text = "Also handled the second thing." }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("first");
    // Type the second line while the turn is still running. The inbox folds it
    // into the turn in flight; it must not start a second one.
    s.term.type_line("and also this");
    s.quiet().await;

    let screen = s.screen();
    assert!(screen.contains("first"), "{screen}");
    assert!(screen.contains("and also this"), "{screen}");
    // The turn-end caption, which is what the person reads: a clean stop's
    // rotating `DONE_LABELS` verb, `Done` for the first turn. Counted by that
    // caption rather than by a variant name, which the screen no longer shows.
    let turn_ends = screen.matches("✻ Done").count();
    assert_eq!(turn_ends, 1, "one turn, not two:\n{screen}");

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// `Ctrl+X` 真的做到了面板上写的那句话:中断,并把排队的话立刻发出去。
///
/// 面板一直写着有这么一下,只是写的是 `esc` —— 而 `esc` 取消之后把排队
/// 的话**丢了**(底座的 `stand_down` 就是这么定的,而屏幕一声不响)。
///
/// 只能端到端钉:键位表只知道这一下解成哪个动作,而这条要钉的是
/// **话真的又发出去了** —— 中间隔着一次取消往返 - 没等到取消终态就
/// 提交会被答 `Busy`,而那正好是把话丢掉的另一种写法。
#[tokio::test]
async fn ctrl_x_interrupts_and_sends_what_was_queued() {
    let dir = scratch("interrupt-and-send");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "answered the queued one" }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("first");
    tokio::time::sleep(Duration::from_millis(400)).await;
    s.term.type_line("QUEUED-x7k");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        s.screen().contains("QUEUED-x7k"),
        "排在面板里了:\n{}",
        s.screen()
    );

    // 面板是「模型的收件箱里还有这么一句」,不是对话。重发之前,这句话
    // 不在 transcript 里。
    assert!(
        !transcript(&s).contains("QUEUED-x7k"),
        "重发之前它只在面板里:\n{}",
        transcript(&s)
    );

    s.term.press(KeyPress::ctrl('x'));

    // 取消落地后重新提交 —— 这一句变成一条真正的用户消息(一条
    // `UserMessage` 事实),这是「话真的又发出去了」唯一不依赖脚本剩几条的
    // 硬证据。
    let mut seen = String::new();
    for _ in 0..300 {
        seen = transcript(&s);
        if seen.contains("QUEUED-x7k") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        seen.contains("QUEUED-x7k"),
        "排队的话被重新发出去了,而不是跟着取消一起消失:\n{}",
        s.screen()
    );
    assert!(
        a_turn_was_interrupted(&s),
        "ctrl-x 真的中断了这一轮,而不是等它自己跑完:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The runtime folds ONE queued line per step, so after the first of three is
/// folded the other two are still waiting — on the panel and in the inbox —
/// and `ctrl-x` sends both. The panel used to empty on the first fold, the
/// stop's withdrawal receipts then matched nothing queued, and the middle line
/// was lost with only the last handed back.
#[tokio::test]
async fn ctrl_x_after_one_queued_line_was_folded_sends_the_rest() {
    let dir = scratch("interrupt-after-fold");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 1" } } ] },
           { text = "two", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "three" },
           { text = "four" },
           { text = "five" }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("first");
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.term.type_line("QUEUED-a1");
    s.term.type_line("QUEUED-b2");
    s.term.type_line("QUEUED-c3");

    // The next step folds the first of them into the turn.
    let mut folded = false;
    for _ in 0..100 {
        if user_messages(&s).iter().any(|m| m == "QUEUED-a1") {
            folded = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(folded, "the first queued line was folded:\n{}", s.screen());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let screen = s.screen();
    assert!(
        screen.contains("QUEUED-b2") && screen.contains("QUEUED-c3"),
        "the two not yet folded are still shown as waiting:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('x'));

    let mut said = Vec::new();
    for _ in 0..300 {
        said = user_messages(&s);
        if said.iter().any(|m| m == "QUEUED-b2") && said.iter().any(|m| m == "QUEUED-c3") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        said.iter().any(|m| m == "QUEUED-b2") && said.iter().any(|m| m == "QUEUED-c3"),
        "both lines still waiting were sent, not dropped: {said:?}\n{}",
        s.screen()
    );
    assert!(
        a_turn_was_interrupted(&s),
        "ctrl-x stopped the turn rather than letting it finish:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Queue two lines behind a running turn and wait until the panel shows both.
///
/// The turn is a five-second `sleep`, so it is still running when the key under
/// test is pressed: what these judge is what happens to words the model has
/// not been handed yet.
async fn queue_two_behind_a_turn(s: &Session, first: &str, second: &str) {
    s.term.type_line("first");
    tokio::time::sleep(Duration::from_millis(400)).await;
    s.term.type_line(first);
    s.term.type_line(second);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let screen = s.screen();
    assert!(
        screen.contains(first) && screen.contains(second),
        "both are in the queue panel:\n{screen}"
    );
}

/// Whether a turn was stopped by a key rather than left to finish: the log
/// records the stop as its own fact. Without this, a key that did nothing at
/// all passes the resend checks — the five-second tool finishes on its own and
/// the queued lines go out as the next turn anyway.
fn a_turn_was_interrupted(s: &Session) -> bool {
    s.client()
        .events()
        .into_iter()
        .any(|logged| matches!(logged.event, SessionEvent::Interrupted { .. }))
}

/// What the person said, message by message, as the log has it.
fn user_messages(s: &Session) -> Vec<String> {
    s.client()
        .events()
        .into_iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::UserMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

/// `esc` stops the turn **and everything queued behind it** — the runtime's
/// `stand_down` is deliberate about that (a stop that let the queue open the
/// next turn is a person watching the agent carry on). But stopping them is
/// not throwing them away: they come back to the composer, in the order they
/// were typed, for the person to send, edit or drop. They used to vanish, with
/// a `没有送达` per line as the only trace.
#[tokio::test]
async fn esc_puts_what_was_queued_back_in_the_composer() {
    let dir = scratch("esc-queued-back");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "two" }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;
    queue_two_behind_a_turn(&s, "QUEUED-a1", "QUEUED-b2").await;

    s.term.press(KeyPress::plain(Key::Esc));
    for _ in 0..300 {
        if composer_text(&s).contains("QUEUED-b2") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    s.quiet().await;
    let field = composer_text(&s);
    let (a, b) = (field.find("QUEUED-a1"), field.find("QUEUED-b2"));
    assert!(
        matches!((a, b), (Some(a), Some(b)) if a < b),
        "both are back in the composer, oldest first:\n{field}"
    );
    assert!(
        !user_messages(&s).iter().any(|m| m.contains("QUEUED")),
        "stopped means not sent: {:?}",
        user_messages(&s)
    );
    assert!(
        !s.screen().contains("没有送达"),
        "a line handed back is not a line that failed:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// `ctrl-c` is the same stop, and hands the queue back the same way — ahead of
/// whatever is being typed, which is where it stood in time.
#[tokio::test]
async fn ctrl_c_puts_what_was_queued_back_ahead_of_the_draft() {
    let dir = scratch("ctrl-c-queued-back");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "two" }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;
    queue_two_behind_a_turn(&s, "QUEUED-c3", "QUEUED-d4").await;
    s.term.type_text("DRAFT-e5");

    s.term.press(KeyPress::ctrl('c'));
    for _ in 0..300 {
        if composer_text(&s).contains("QUEUED-d4") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    s.quiet().await;
    let field = composer_text(&s);
    let at = |needle: &str| field.find(needle);
    assert!(
        matches!(
            (at("QUEUED-c3"), at("QUEUED-d4"), at("DRAFT-e5")),
            (Some(c), Some(d), Some(e)) if c < d && d < e
        ),
        "the queue comes back ahead of the draft, which is kept:\n{field}"
    );
    assert!(
        !user_messages(&s).iter().any(|m| m.contains("QUEUED")),
        "{:?}",
        user_messages(&s)
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// What `Ctrl+X` sent after the first line is queued again, and a stop hands
/// it back like any other.
///
/// Only the first resent line opens the turn; the runtime takes one message a
/// step, so the rest wait in its inbox exactly as lines typed mid-turn do. They
/// were sent untracked, so an `esc` before they were folded in withdrew them
/// with nothing to give them back to — `没有送达`, and gone.
#[tokio::test]
async fn what_ctrl_x_resent_and_was_still_waiting_comes_back_on_esc() {
    let dir = scratch("ctrl-x-then-esc");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "two", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "three" }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;
    queue_two_behind_a_turn(&s, "QUEUED-h8", "QUEUED-i9").await;

    s.term.press(KeyPress::ctrl('x'));
    for _ in 0..300 {
        if user_messages(&s).iter().any(|m| m.contains("QUEUED-h8")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Judged before the esc, which interrupts too: without this, a `ctrl-x`
    // that did nothing passes — the five-second tool finishes, the next step
    // folds `h8` in, and the esc below does all the stopping.
    assert!(
        a_turn_was_interrupted(&s),
        "ctrl-x stopped the turn, before any esc:\n{}",
        s.screen()
    );
    assert!(
        s.screen().contains("QUEUED-i9"),
        "the second line is shown waiting again:\n{}",
        s.screen()
    );
    s.term.press(KeyPress::plain(Key::Esc));
    for _ in 0..300 {
        if composer_text(&s).contains("QUEUED-i9") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    s.quiet().await;
    assert!(
        composer_text(&s).contains("QUEUED-i9"),
        "it comes back to the composer:\n{}",
        s.screen()
    );
    assert!(
        !user_messages(&s).iter().any(|m| m.contains("QUEUED-i9")),
        "{:?}",
        user_messages(&s)
    );
    assert!(!s.screen().contains("没有送达"), "{}", s.screen());

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The turns a stop ended, by the log.
fn interrupted_turns(s: &Session) -> Vec<u64> {
    s.client()
        .events()
        .into_iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::Interrupted { turn, .. } => Some(turn),
            _ => None,
        })
        .collect()
}

/// The turn each user message was said in, as the log has it.
fn user_turns(s: &Session) -> Vec<(u64, String)> {
    s.client()
        .events()
        .into_iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::UserMessage { turn, text, .. } => Some((turn, text)),
            _ => None,
        })
        .collect()
}

/// A queued line the turn had already taken out of the inbox — claimed at a
/// step boundary and held in `PreStep` (a picture being recognised) — when
/// `ctrl-x` landed is sent again like the rest, and first, where it was typed.
///
/// The stop withdraws only what is still in the inbox, and this one was not:
/// it was committed into the turn being stopped, which then ended before
/// asking the model anything. It sat in the conversation unanswered while the
/// line typed after it went out and was answered — "stopped, not sent" for the
/// one the person had said first.
#[tokio::test]
async fn ctrl_x_sends_again_a_queued_line_the_turn_was_still_taking_in() {
    let dir = scratch("ctrl-x-claimed");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 1" } } ] },
           { text = "two" }, { text = "three" }, { text = "four" }, { text = "five" }"#,
    );
    let s = start(tree(&dir, &script, &[SLOW_PRE_STEP])).await;
    let task = s.open().await;

    s.term.type_line("first");
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.term.type_line("SLOW-k4m");
    s.term.type_line("QUEUED-k5n");
    held_at_pre_step("SLOW-k4m").await;

    s.term.press(KeyPress::ctrl('x'));
    for _ in 0..300 {
        let said = user_messages(&s);
        if said.iter().any(|m| m == "QUEUED-k5n") && said.iter().any(|m| m == "SLOW-k4m") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stopped = interrupted_turns(&s);
    assert!(
        !stopped.is_empty(),
        "ctrl-x stopped the turn:\n{}",
        s.screen()
    );
    let said = user_turns(&s);
    let at = |needle: &str| said.iter().position(|(_, text)| text == needle);
    assert!(
        said.iter()
            .filter(|(_, text)| text == "SLOW-k4m")
            .all(|(turn, _)| !stopped.contains(turn)),
        "the line being taken in did not go into the stopped turn: {said:?}"
    );
    assert!(
        matches!((at("SLOW-k4m"), at("QUEUED-k5n")), (Some(a), Some(b)) if a < b),
        "both went out again, in the order they were typed: {said:?}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The same line, stopped with `esc`: back to the composer with the rest, ahead
/// of them — not left in the conversation of a turn that never answered it.
#[tokio::test]
async fn esc_hands_back_a_queued_line_the_turn_was_still_taking_in() {
    let dir = scratch("esc-claimed");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 1" } } ] },
           { text = "two" }, { text = "three" }"#,
    );
    let s = start(tree(&dir, &script, &[SLOW_PRE_STEP])).await;
    let task = s.open().await;

    s.term.type_line("first");
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.term.type_line("SLOW-p2q");
    s.term.type_line("QUEUED-p3r");
    held_at_pre_step("SLOW-p2q").await;

    s.term.press(KeyPress::plain(Key::Esc));
    for _ in 0..300 {
        if composer_text(&s).contains("SLOW-p2q") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    s.quiet().await;
    let field = composer_text(&s);
    assert!(
        matches!(
            (field.find("SLOW-p2q"), field.find("QUEUED-p3r")),
            (Some(a), Some(b)) if a < b
        ),
        "both are back in the composer, in the order they were typed:\n{field}"
    );
    assert!(
        !user_messages(&s)
            .iter()
            .any(|m| m.contains("SLOW-p2q") || m.contains("QUEUED-p3r")),
        "stopped means not in the conversation: {:?}",
        user_messages(&s)
    );
    assert!(!s.screen().contains("没有送达"), "{}", s.screen());

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A `ctrl-x` that found nothing to withdraw does not turn a later cancel —
/// one no key asked for, like a model switch reconfiguring the turn — into a
/// resend: what that cancel withdraws comes back to the composer.
///
/// "Nothing to withdraw" is the screen not having heard the fold yet: the line
/// is still on its panel when the key goes down, but the runtime has already
/// handed it to the model. Here the connection never passes the fold on, which
/// makes that moment last. The key then asked for a resend nothing consumed,
/// and the flag stood until the next cancel of any kind.
#[tokio::test]
async fn a_ctrl_x_that_withdrew_nothing_does_not_make_a_later_cancel_resend() {
    let dir = scratch("ctrl-x-stale");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 1" } } ] },
           { text = "two", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "three", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "four" }, { text = "five" }, { text = "six" }"#,
    );
    let s = start_with_connection(tree(&dir, &script, &[]), |connection| {
        let atomcode_host_api::HostConnection {
            session,
            commands,
            mut events,
            control,
        } = connection;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if matches!(event, atomcode_kernel::event::AgentEvent::Steered { .. }) {
                    continue;
                }
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        atomcode_host_api::HostConnection {
            session,
            commands,
            events: rx,
            control,
        }
    })
    .await;
    let task = s.open().await;

    s.term.type_line("first");
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.term.type_line("LATE-s1");
    for _ in 0..100 {
        if user_messages(&s).iter().any(|m| m == "LATE-s1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        s.screen().contains("LATE-s1"),
        "the fold was not heard, so the line is still listed:\n{}",
        s.screen()
    );
    s.term.press(KeyPress::ctrl('x'));
    s.quiet().await;
    assert!(a_turn_was_interrupted(&s), "{}", s.screen());

    s.term.type_line("second");
    tokio::time::sleep(Duration::from_millis(400)).await;
    s.term.type_line("LATE-s2");
    tokio::time::sleep(Duration::from_millis(400)).await;
    s.client().cancel();
    for _ in 0..300 {
        if composer_text(&s).contains("LATE-s2") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    s.quiet().await;
    assert!(
        composer_text(&s).contains("LATE-s2"),
        "a cancel no key asked for hands the line back:\n{}",
        s.screen()
    );
    assert!(
        !user_messages(&s).iter().any(|m| m == "LATE-s2"),
        "and does not send it: {:?}",
        user_messages(&s)
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// When the lead's turn events do not arrive — its status going idle is all
/// the screen hears — a stop's withdrawn lines are still settled there, not
/// held for a turn end that is not coming.
#[tokio::test]
async fn a_stop_settles_on_idle_when_no_turn_end_arrives() {
    let dir = scratch("settle-on-idle");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "two" }"#,
    );
    let s = start_with_connection(tree(&dir, &script, &[]), |connection| {
        let atomcode_host_api::HostConnection {
            session,
            commands,
            mut events,
            control,
        } = connection;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            use atomcode_kernel::event::AgentEvent;
            while let Some(event) = events.recv().await {
                if matches!(
                    event,
                    AgentEvent::TurnStarted { .. }
                        | AgentEvent::TurnComplete { .. }
                        | AgentEvent::Cancelled
                ) {
                    continue;
                }
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        atomcode_host_api::HostConnection {
            session,
            commands,
            events: rx,
            control,
        }
    })
    .await;
    let task = s.open().await;
    queue_two_behind_a_turn(&s, "QUEUED-t1", "QUEUED-t2").await;

    s.term.press(KeyPress::plain(Key::Esc));
    for _ in 0..300 {
        if composer_text(&s).contains("QUEUED-t2") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    s.quiet().await;
    let field = composer_text(&s);
    assert!(
        matches!(
            (field.find("QUEUED-t1"), field.find("QUEUED-t2")),
            (Some(a), Some(b)) if a < b
        ),
        "both are back in the composer:\n{field}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// `Ctrl+X` sends what was queued **as it was queued**: one message each, in
/// order — the same shape they would have had folding into the turn — not
/// glued into one message the person never wrote. And a queued line that
/// carried a picture still carries it: the runtime dropped the queued send,
/// picture and all, so the resend has to bring it again.
#[tokio::test]
async fn ctrl_x_sends_each_queued_message_on_its_own_with_its_picture() {
    let dir = scratch("ctrl-x-each");
    let script = replay_vision(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "two" }, { text = "three" }, { text = "four" }"#,
        true,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;
    s.term.type_line("first");
    tokio::time::sleep(Duration::from_millis(400)).await;
    s.term.set_clipboard_image(screenshot("queued"));
    s.term.press(KeyPress::ctrl('v'));
    s.term.type_line("QUEUED-f6 看图");
    s.term.type_line("QUEUED-g7");
    tokio::time::sleep(Duration::from_millis(400)).await;

    s.term.press(KeyPress::ctrl('x'));
    for _ in 0..300 {
        if user_messages(&s).iter().any(|m| m.contains("QUEUED-g7")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    s.quiet().await;
    let said = user_messages(&s);
    let f6 = said.iter().position(|m| m.contains("QUEUED-f6"));
    let g7 = said.iter().position(|m| m.contains("QUEUED-g7"));
    assert!(
        matches!((f6, g7), (Some(f), Some(g)) if f < g),
        "two messages, in the order they were queued — not one glued together: {said:?}"
    );
    let with_picture = s
        .client()
        .events()
        .into_iter()
        .find_map(|logged| match logged.event {
            SessionEvent::UserMessage { text, images, .. } if text.contains("QUEUED-f6") => {
                Some(images.len())
            }
            _ => None,
        });
    assert!(
        a_turn_was_interrupted(&s),
        "ctrl-x stopped the turn:\n{}",
        s.screen()
    );
    assert_eq!(with_picture, Some(1), "the queued picture went with it");

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_line_typed_mid_turn_is_shown_until_the_model_is_handed_it() {
    // The gap this panel exists for, and it is only visible from the outside:
    // a message typed during a turn goes to the agent's inbox and is folded in
    // at the next ROUND boundary, so between the two there is no `UserMessage`
    // fact for the transcript to fold and the words are nowhere on screen —
    // already sent, no longer in the field, not yet in the conversation.
    //
    // The slow tool is what holds that window open. Measured here rather than
    // assumed: the words land at the round boundary, NOT at the end of the turn,
    // so a panel that stayed until `TurnComplete` would draw the same sentence
    // twice for as long as the rest of the turn took.
    let dir = scratch("steering-panel");
    let script = replay(
        r#"{ text = "one", calls = [ { name = "bash", args = { command = "sleep 3" } } ] },
           { text = "two", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "three" }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("first");
    tokio::time::sleep(Duration::from_millis(400)).await;
    s.term.type_line("STEER-ME");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let waiting = s.screen();
    assert!(
        waiting.contains("STEER-ME"),
        "the person's own words must be on screen while they are in flight:\n{waiting}"
    );
    assert!(
        waiting.contains("运行中"),
        "…and this is the mid-turn window, not the end of it — the words here \
         are the in-flight call row's own note. The status line says it with the \
         cat instead (`status::WORKING_FRAMES`), whose segment is past the right \
         edge at 80 columns:\n{waiting}"
    );

    // Past the round boundary: `sleep 3` is done, the fold has happened, the
    // model has the words, and the next step has opened its own long sleep.
    tokio::time::sleep(Duration::from_millis(2900)).await;
    let folded = s.screen();
    assert!(
        folded.contains("STEER-ME"),
        "the transcript owns the words from here on:\n{folded}"
    );
    assert!(
        folded.contains("运行中"),
        "still inside the turn, so this is the handover and not the end — the \
         in-flight call row again, not the status line:\n{folded}"
    );
    // One copy, not two. This is the assertion the `Steered`-timed clear exists
    // for: the panel leaves as the block arrives.
    assert_eq!(
        folded.matches("STEER-ME").count(),
        1,
        "the panel must be gone by the time the transcript draws the words:\n{folded}"
    );

    s.quiet().await;
    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// Multi-threaded on purpose. The driver commits the turn's last fact and
// marks the agent idle in one synchronous stretch; on the single-threaded test
// runtime the UI task cannot run between the two, so the race this test exists
// for cannot happen there — and a test that cannot fail proves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_the_model_never_answers_says_so_and_stops_spinning() {
    // No network, no key, a dead endpoint: every one of them reaches the loop
    // as a request that fails, and the person is owed two things — the cause
    // on the screen, and a status line that stops saying the agent is working.
    // The message is the shape a provider really produces: one sentence far
    // wider than the 80 columns this screen has.
    let dir = scratch("failed");
    let fail = r#"{ fail = "open failed: error sending request for url (https://openrouter.ai/api/v1/chat/completions): client error (Connect): dns error: failed to lookup address information: nodename nor servname provided, or not known" }"#;
    // Wide enough that the status line's activity segment is on screen: at 80
    // columns the working directory pushes it off the right edge, and an
    // assertion about text that was never drawn passes for the wrong reason.
    let wide = "[[patch]]\nid = \"surface\"\nconfig = { width = 160, height = 24 }\n";
    // Hold the driver between "the turn ended" being committed and the agent
    // being marked idle (`HoldTurnEnd`). A UI that reads the agent's status on
    // that fact and never looks again is caught.
    let hold = "[[insert]]\nname = \"test-hold-turn-end\"\n";
    let s = start(tree(&dir, &replay(fail), &[wide, hold])).await;
    let task = s.open().await;

    s.term.type_line("hello?");
    s.quiet().await;

    let screen = s.screen();
    assert!(screen.contains("已中断"), "the outcome:\n{screen}");
    assert!(
        screen.contains("nodename nor servname"),
        "the cause, wrapped rather than dropped:\n{screen}"
    );
    // The status line says "working" with the cat now, so this control names the
    // cat rather than the words it replaced: `运行中` is *also* what a pending
    // tool row says (`content.rs`), which would have kept this passing for a
    // reason that has nothing to do with the status line.
    for frame in atomcode_tui::modules::status::WORKING_FRAMES {
        assert!(
            !screen.contains(frame),
            "the status line must not claim a finished turn is running:\n{screen}"
        );
    }
    assert!(
        !screen.contains("运行中"),
        "and no tool row is left saying it either:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_half_typed_line_survives_the_model_streaming_over_it() {
    let dir = scratch("halftyped");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "a fairly long answer here" }"#),
        &[],
    ))
    .await;
    let task = s.open().await;

    s.term.type_line("go");
    s.term.type_text("wait, che");
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("wait, che"),
        "the half-typed line must not be eaten by the stream:\n{screen}"
    );
    assert!(screen.contains("a fairly long answer"), "{screen}");

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn folding_changes_what_is_shown_and_not_what_was_said() {
    let dir = scratch("fold");
    std::fs::write(dir.join("a.rs"), "x").unwrap();
    let script = replay(
        r#"{ text = "Reading.", calls = [ { name = "read_file", args = { file_path = "a.rs" } } ] },
           { text = "answer" }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;
    s.term.type_line("hi");
    s.quiet().await;

    let before = s.screen();
    // A finished call folds itself to one row (`Full` auto-collapses what has
    // stopped running). The cycle is `Full → Head → Each → Group`; the first
    // press moves to `Head`, which previews the call in full again, so the one
    // gesture visibly re-expands the row that was folded.
    s.term.press(KeyPress::ctrl('t')); // full (auto-folded) -> head (previewed)
    s.quiet().await;
    let after = s.screen();
    assert_ne!(before, after, "folding must change the screen");
    assert!(
        after.contains("answer"),
        "and must not lose content:\n{after}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_panel_is_on_screen_because_a_row_mounted_it() {
    // The whole point of panels being rows. Nothing in the launcher, the key
    // map or `assemble` knows the mascot exists — one line of config does.
    let dir = scratch("mascot-row");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[patch]]\nid = \"tui-panel-mascot\"\ndisabled = false"],
    ))
    .await;
    let task = s.open().await;
    s.term.type_line("hi");
    s.quiet().await;
    assert!(
        s.term.last().unwrap().part("mascot").is_some(),
        "the row put itself on screen, through the same LayoutOp everything else uses"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Stop a running turn with an empty composer and the prompt that was running
/// comes back into the field, ready to edit and resend — and the composer says
/// it was stopped.
#[tokio::test]
async fn esc_hands_the_running_prompt_back_to_the_empty_composer() {
    let dir = scratch("esc-restore");
    let script = replay(
        r#"{ text = "Working.", calls = [ { name = "bash", args = { command = "sleep 0.4" } } ] },
           { text = "Done." }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("fix the parser");
    tokio::time::sleep(Duration::from_millis(120)).await;
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;

    let screen = s.screen();
    // Twice now: once in the transcript as what was asked, once back in the
    // composer as what to resend. Without the hand-back it would be there once.
    assert_eq!(
        screen.matches("fix the parser").count(),
        2,
        "the stopped prompt is back in the composer:\n{screen}"
    );
    assert!(
        screen.contains("已中断"),
        "the composer says it was stopped:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Stop a running turn while you were already typing something else and the
/// draft is left exactly as it is — the prompt is not handed back on top of it.
#[tokio::test]
async fn esc_keeps_a_typed_draft_and_does_not_hand_the_prompt_back() {
    let dir = scratch("esc-keep-draft");
    let script = replay(
        r#"{ text = "Working.", calls = [ { name = "bash", args = { command = "sleep 0.4" } } ] },
           { text = "Done." }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("run it");
    tokio::time::sleep(Duration::from_millis(120)).await;
    // Typed while the turn is still in flight — it lands in the composer, not
    // the model (see `typing_during_a_turn_is_folded_into_it`).
    s.term.type_text("half a thought");
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("half a thought"),
        "the draft is kept as it was:\n{screen}"
    );
    assert_eq!(
        screen.matches("run it").count(),
        1,
        "the sent prompt stays in the transcript, not re-added over the draft:\n{screen}"
    );
    assert!(
        screen.contains("已中断"),
        "and it says it was stopped:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// On an idle screen a draft takes two taps of Esc to clear — the first arms,
/// the second clears — so a stray press does not wipe what you were writing.
#[tokio::test]
async fn the_shell_line_editing_chords_work_on_a_real_draft() {
    // The table says which action each chord resolves to; this says the
    // actions do anything. Bound to a handler that was never written, every
    // one of these keys is silently dead — which is what `Delete` was.
    let dir = scratch("line-editing");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_text("alpha omega");
    s.quiet().await;

    // ctrl-a to the start, then Delete takes the character the caret is on.
    s.term.press(KeyPress::ctrl('a'));
    s.term.press(KeyPress::plain(Key::Delete));
    s.quiet().await;
    assert!(
        s.screen().contains("lpha omega"),
        "ctrl-a went to the start and Delete took the character there:\n{}",
        s.screen()
    );

    // ^H is what some terminals send for backspace. At the start there is
    // nothing behind the caret, so it must be a no-op rather than an error.
    s.term.press(KeyPress::ctrl('h'));
    s.quiet().await;
    assert!(s.screen().contains("lpha omega"), "{}", s.screen());

    // ctrl-e to the end, then ^H takes the character behind it.
    s.term.press(KeyPress::ctrl('e'));
    s.term.press(KeyPress::ctrl('h'));
    s.quiet().await;
    assert!(
        s.screen().contains("lpha omeg") && !s.screen().contains("lpha omega"),
        "ctrl-e went to the end and ^H backspaced there:\n{}",
        s.screen()
    );

    // ctrl-a then ctrl-k cuts everything from the caret on.
    s.term.press(KeyPress::ctrl('a'));
    s.term.press(KeyPress::ctrl('k'));
    s.quiet().await;
    assert!(
        !s.screen().contains("lpha omeg"),
        "ctrl-k cut the rest of the line:\n{}",
        s.screen()
    );
    task.abort();
}

/// `Ctrl+R` finds a thing said earlier by a word that was in it, and Enter only
/// accepts it into the composer.
///
/// The unit judgements in `crate::search` all call the mode directly, so every
/// one of them stays green with the chord unbound and the routing arm missing —
/// the way `Delete` was silently dead for months. This is the one that presses
/// the key.
#[tokio::test]
async fn ctrl_r_finds_an_earlier_line_by_a_word_in_it() {
    let dir = scratch("reverse-search");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "a" }, { text = "b" }, { text = "c" }"#),
        &[],
    ))
    .await;
    let task = s.open().await;
    s.quiet().await;

    // Three things said, so there is a history with a word to find in it.
    s.term.type_line("run the parser tests");
    s.quiet().await;
    s.term.type_line("fix the width arithmetic");
    s.quiet().await;
    s.term.type_line("check the parser again");
    s.quiet().await;

    // Open the search and type a word that is in two of them.
    s.term.press(KeyPress::ctrl('r'));
    s.term.type_text("parser");
    s.quiet().await;
    let screen = s.screen();
    assert!(
        screen.contains("搜索 'parser'"),
        "the shoulder says what is being searched for:\n{screen}"
    );
    assert!(
        screen.contains("check the parser again"),
        "and the composer shows the newest hit:\n{screen}"
    );

    // Again steps to the older of the two.
    s.term.press(KeyPress::ctrl('r'));
    s.quiet().await;
    assert!(
        s.screen().contains("run the parser tests"),
        "a second Ctrl+R steps further back:\n{}",
        s.screen()
    );

    // Enter accepts it. It must NOT send: the reply to a fourth turn would be
    // on screen if it had, and the search caption must be gone.
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    let screen = s.screen();
    assert!(
        !screen.contains("搜索 '"),
        "accepting closes the search:\n{screen}"
    );
    assert!(
        screen.contains("run the parser tests"),
        "and leaves the hit in the composer to edit:\n{screen}"
    );
    task.abort();
}

/// Esc out of a search gives back the draft it was opened over. A chord pressed
/// by accident has to cost nothing — that is the whole reason the draft is
/// stashed rather than overwritten.
#[tokio::test]
async fn esc_out_of_a_search_gives_the_draft_back() {
    let dir = scratch("reverse-search-esc");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "a" }, { text = "b" }"#),
        &[],
    ))
    .await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("something said earlier");
    s.quiet().await;

    s.term.type_text("half a thought");
    s.quiet().await;
    s.term.press(KeyPress::ctrl('r'));
    s.quiet().await;
    assert!(
        s.screen().contains("something said earlier"),
        "the search opened on the newest entry:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    assert!(
        s.screen().contains("half a thought"),
        "Esc gave the draft back:\n{}",
        s.screen()
    );
    task.abort();
}

/// A picture on the clipboard is offered in the tip row above the composer — the
/// moment it is worth saying is when the person comes back to the terminal with
/// one — and the offer goes once the picture is taken. The same picture still
/// on the clipboard is not offered again: it is the one already on the line.
#[tokio::test]
async fn a_picture_on_the_clipboard_is_offered_until_it_is_taken() {
    let dir = scratch("clipboard-hint");
    let s = start(tree(&dir, &replay_vision(r#"{ text = "ok" }"#, true), &[])).await;
    let task = s.open().await;
    s.quiet().await;
    s.term.focus(true);
    s.quiet().await;
    assert!(
        !s.screen().contains("剪贴板有图片"),
        "nothing on the clipboard, nothing offered:\n{}",
        s.screen()
    );

    s.term.set_clipboard_image(screenshot("offered"));
    s.term.focus(true);
    until(&s, "剪贴板有图片").await;
    assert!(
        s.screen().contains("ctrl+v"),
        "it says which key takes it here:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('v'));
    s.quiet().await;
    assert!(s.screen().contains("[Image #1]"), "{}", s.screen());
    assert!(
        !s.screen().contains("剪贴板有图片"),
        "taken, so no longer offered:\n{}",
        s.screen()
    );

    s.term.focus(true);
    s.quiet().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !s.screen().contains("剪贴板有图片"),
        "the same picture is not offered twice:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The door most people use: `Cmd+V`, which the terminal swallows and delivers
/// as a bracketed paste rather than as the `Ctrl+V` key. Nothing looks at the
/// clipboard while it arrives — a paste is not a keystroke — so the look the
/// *next* keystroke starts is the first to see that picture, and it has to
/// recognize it as the one already on the line. The reported screen was the
/// offer appearing after the paste, naming a picture already in the composer.
///
/// No focus event anywhere: the picture was copied with the terminal already in
/// front, which is the case where no look has run at all before the paste.
#[tokio::test]
async fn a_picture_pasted_with_cmd_v_is_not_offered_back_afterwards() {
    let dir = scratch("cmd-v-offer");
    let s = start(tree(&dir, &replay_vision(r#"{ text = "ok" }"#, true), &[])).await;
    let task = s.open().await;
    s.quiet().await;
    s.term.set_clipboard_image(screenshot("cmd-v"));

    // An empty bracketed paste is what a screenshot arrives as: the terminal
    // has no text to hand over, and the picture is recovered from the clipboard.
    s.term.paste("");
    s.quiet().await;
    assert!(s.screen().contains("[Image #1]"), "{}", s.screen());

    // The look the next keystroke starts reads that same picture — but looks are
    // spaced (`LOOK_EVERY`), so the keystroke has to come after that spacing or
    // no look happens and this judgement would pass on a screen nothing looked
    // at. The spacing is why the reported bug showed up on typing rather than at
    // the paste: the paste itself looks at nothing.
    tokio::time::sleep(Duration::from_millis(1_600)).await;
    s.term.type_text("x");
    s.quiet().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !s.screen().contains("剪贴板有图片"),
        "the picture already on the line is not offered back:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// `/paste` is the way in for a clipboard picture on a terminal that eats
/// `Ctrl+V` — Windows Terminal, PuTTY. Typed as a command, so nothing in the
/// key layer can swallow it.
///
/// Pressed rather than unit-judged because this command was **text-only** until
/// now: the `attach` judgements next to `from_clipboard` all pass with the
/// command still returning `Action::Paste`, which is exactly how the only road
/// to a clipboard picture on Windows came to be the one chord Windows eats.
#[tokio::test]
async fn slash_paste_attaches_the_clipboard_picture() {
    let dir = scratch("slash-paste-image");
    let s = start(tree(&dir, &replay_vision(r#"{ text = "ok" }"#, false), &[])).await;
    let task = s.open().await;
    s.quiet().await;
    s.term.set_clipboard_image(screenshot("slash-paste"));

    // No Ctrl+V anywhere: this is the road for a terminal that has none.
    s.term.type_line("/paste");
    s.quiet().await;
    assert!(
        s.screen().contains("[Image #1]"),
        "`/paste` attached the clipboard picture:\n{}",
        s.screen()
    );
    task.abort();
}

/// And with no picture there, `/paste` is still the text command it was.
#[tokio::test]
async fn slash_paste_still_pastes_text_when_there_is_no_picture() {
    let dir = scratch("slash-paste-text");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;
    s.term.set_clipboard_text("a line someone copied");

    s.term.type_line("/paste");
    s.quiet().await;
    assert!(
        s.screen().contains("a line someone copied"),
        "text still goes in as text:\n{}",
        s.screen()
    );
    task.abort();
}

/// A picture attached to a **command** goes nowhere, and the screen says so.
///
/// No command carries pictures, and the submit path drains them off the
/// composer before it decides the line was a command — so a screenshot attached
/// and then `/goal 照着这张图做` vanished with no word about it, which is the
/// one thing the attachment subsystem exists to prevent. Until some command
/// does carry them, being told is the fix.
#[tokio::test]
async fn a_picture_attached_to_a_command_is_not_dropped_in_silence() {
    let dir = scratch("command-with-picture");
    let s = start(tree(&dir, &replay_vision(r#"{ text = "ok" }"#, true), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    // The command first, then the picture: a marker in front of the slash would
    // make the line ordinary prose, which is a different thing entirely.
    s.term.type_text("/keys ");
    s.term.set_clipboard_image(screenshot("for-a-command"));
    s.term.press(KeyPress::ctrl('v'));
    s.quiet().await;
    assert!(s.screen().contains("[Image #1]"), "{}", s.screen());

    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    let screen = s.screen();
    assert!(
        screen.contains("命令带不了图片"),
        "the screen says the picture went nowhere:\n{screen}"
    );
    task.abort();
}

#[tokio::test]
async fn an_idle_draft_takes_two_taps_of_esc_to_clear() {
    let dir = scratch("esc-idle-clear");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_text("scratch note");
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    assert!(
        s.screen().contains("scratch note"),
        "one tap does not clear the draft:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    assert!(
        !s.screen().contains("scratch note"),
        "the second tap clears it:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn esc_stops_the_turn_and_the_next_one_still_runs() {
    let dir = scratch("cancel");
    // A tool that genuinely awaits. With an all-in-memory script the whole turn
    // finishes in microseconds and a keystroke can never land inside it — the
    // test would pass or fail on scheduler luck rather than on behaviour.
    let script = replay(
        r#"{ text = "Working.", calls = [ { name = "bash", args = { command = "sleep 0.4" } } ] },
           { text = "Working.", calls = [ { name = "bash", args = { command = "sleep 0.4" } } ] },
           { text = "Done." }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("go");
    tokio::time::sleep(Duration::from_millis(120)).await;
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    assert!(s.screen().contains("已中断"), "{}", s.screen());

    s.term.type_line("again");
    s.quiet().await;
    let screen = s.screen();
    assert!(
        screen.contains("again"),
        "an agent asked to stop once must still work:\n{screen}"
    );
    assert!(
        screen.contains("Done."),
        "and the next turn really runs:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Esc on an idle screen is not a stop, and must not be allowed to say it was.
///
/// The claim it used to make could not be taken back. Asking an idle agent to
/// cancel is a no-op by design — and an unchanged `activity` sends nothing back,
/// while a changed one would send `StatusChanged`, which on this connection is
/// only read for a member's screen. So `Stopping` written here stood until the
/// next turn finished.
///
/// Two wrongs came out of that one stale write, and this test is both of them:
/// the words on the screen and what the *next* submission was read as. That
/// second one is why this is not a cosmetic test — the front end decides "this
/// is steering" by asking whether the agent is busy, so an idle screen left
/// saying `Stopping` put the next question in the steering panel, as work the
/// model was about to be handed, when it in fact opened a fresh turn.
///
/// The negative control is the `stopping` write itself: put
/// `self.host.set_activity(Activity::Stopping)` back in `Action::Escape` and both
/// halves go red — the status line reads 停止中, and the steering bar is up while
/// the turn's tool runs.
#[tokio::test]
async fn esc_on_an_idle_screen_stops_nothing_and_says_nothing() {
    let dir = scratch("esc-idle");
    // A turn that stays in flight long enough to be looked at: with an in-memory
    // script the whole thing is over in microseconds and "no steering bar" would
    // be a claim about a moment that never existed.
    let script = replay(
        r#"{ text = "Working.", calls = [ { name = "bash", args = { command = "sleep 0.4" } } ] },
           { text = "Ready." }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    let idle = s.term.last().expect("a frame");
    assert!(
        idle.part("live").is_none(),
        "esc on an idle screen raised a live line:\n{}",
        s.screen()
    );
    assert!(
        !idle
            .part("status")
            .expect("the status line")
            .lines
            .iter()
            .any(|l| l.plain().contains("停止中")),
        "esc on an idle screen left the status line saying 停止中:\n{}",
        s.screen()
    );

    // And the next turn is a turn: its words are the first message of it, not a
    // steering bar for something already on its way to the model.
    s.term.type_line("hello");
    until(&s, "正在运行 1 个工具").await;
    assert!(
        s.term.last().expect("a frame").part("steering").is_none(),
        "the words went into the steering panel — the screen still thought the \
         agent was stopping, so a new turn was read as a follow-up into one:\n{}",
        s.screen()
    );

    s.quiet().await;
    assert!(s.screen().contains("Ready."), "{}", s.screen());
    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The tree the TUI is meant to run in: it asks before a risky call, and it is
/// the thing being asked.
fn asking(root: &std::path::Path, script: &str) -> Setup {
    tree(
        root,
        script,
        &[
            "[[patch]]\nid = \"approval\"\ndisabled = true\n",
            "[[patch]]\nid = \"approval-interactive\"\ndisabled = false\n",
        ],
    )
}

/// Wait for something to appear on screen, or fail saying what was there
/// instead. Used where `quiet` cannot be: a turn blocked on a question never
/// goes quiet until it is answered.
async fn until(s: &Session, text: &str) {
    for _ in 0..400 {
        if s.screen().contains(text) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("`{text}` never appeared:\n{}", s.screen());
}

/// Wait for something to *stop* being on screen — the shape a filter needs,
/// where what is asserted is what is no longer listed.
async fn until_gone(s: &Session, text: &str) {
    for _ in 0..400 {
        if !s.screen().contains(text) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("`{text}` never went away:\n{}", s.screen());
}

#[tokio::test]
async fn the_live_line_says_what_the_turn_is_doing_while_it_runs() {
    // The one thing a screen of blocks cannot say: a turn waiting on a tool and
    // a turn that has finished look alike, because the block that would tell
    // them apart has not arrived yet. The line is drawn from the facts *and*
    // from the host's clock, so this asserts both — a line without the seconds
    // is a line whose opening reading never got stamped.
    let dir = scratch("live-line");
    let script = replay(
        r#"{ text = "Reading it.", calls = [ { name = "bash", args = { command = "sleep 0.6" } } ] },
           { text = "Done." }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("go");
    until(&s, "正在运行 1 个工具").await;
    // The figures are a reading the provider reported, and they belong to the
    // turn in flight: 100 tokens of context, 20 out of this round. Waiting for
    // the reading rather than assuming it beats the tool call — the row is drawn
    // from the log, and the log says which of the two came first.
    until(&s, "入 100").await;
    let live = s
        .term
        .last()
        .and_then(|f| f.part("live").map(|p| p.lines.clone()))
        .expect("the live line is on screen while the tool runs");
    let said: String = live.iter().map(|l| l.plain()).collect();
    assert!(said.contains("正在运行 1 个工具"), "{said}");
    assert!(
        said.contains("耗时 "),
        "and says how long it has been running: {said}"
    );
    assert!(
        said.contains("入 100") && said.contains("出 20"),
        "and what it has cost so far: {said}"
    );

    s.quiet().await;
    let done = s.screen();
    assert!(
        !done.contains("正在运行"),
        "the row goes with the turn it was about:\n{done}"
    );
    assert!(done.contains("Done."), "{done}");

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_risky_call_is_asked_about_on_screen_and_an_allow_lets_it_run() {
    let dir = scratch("approve");
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "written" } } ] },
           { text = "Done." }"#,
    );
    let s = start(asking(&dir, &script)).await;
    let task = s.open().await;

    s.term.type_line("write it");
    // Wait for the question rather than for quiet: the turn is deliberately
    // blocked on the answer, so quiet will never come until we give one.
    until(&s, "esc 拒绝").await;
    let card = s.screen();
    assert!(card.contains("write_file"), "which tool:\n{card}");
    assert!(card.contains("out.txt"), "and what it would do:\n{card}");
    assert!(
        !dir.join("out.txt").exists(),
        "nothing may run before it is approved"
    );

    s.term.press(KeyPress::ch('1'));
    s.quiet().await;
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "written",
        "an approved call runs"
    );
    let screen = s.screen();
    assert!(
        screen.contains("→ 允许一次"),
        "the answer is on the record:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn esc_declines_and_the_model_is_told_rather_than_the_turn_dying() {
    let dir = scratch("decline");
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "no" } } ] },
           { text = "Understood." }"#,
    );
    let s = start(asking(&dir, &script)).await;
    let task = s.open().await;

    s.term.type_line("write it");
    until(&s, "esc 拒绝").await;

    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;

    assert!(!dir.join("out.txt").exists(), "a refused call must not run");
    let screen = s.screen();
    // Refusing is *returning a result*: the model learns why and the turn
    // finishes, instead of dying with a dangling call.
    assert!(
        screen.contains("→ 拒绝"),
        "the refusal is recorded:\n{screen}"
    );
    assert!(
        screen.contains("Understood."),
        "the turn carried on:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The model's own `request_user_input`, mounted the way the coding runtime
/// mounts it: the capabilities tool as it ships, asking through the driver's
/// request channel — which is the screen.
struct RequestUserInputRow;

#[async_trait]
impl Plugin for RequestUserInputRow {
    fn name(&self) -> &'static str {
        "test-request-user-input"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        atomcode_harness::plugins::tools::mount(
            ctx,
            vec![
                Arc::new(atomcode_capabilities::tools::request_user_input::RequestUserInputTool)
                    as Arc<dyn atomcode_kernel::tool::Tool>,
            ],
        )
    }
}

const REQUEST_USER_INPUT_LAYER: &str = "[[insert]]\nname = \"test-request-user-input\"\n";

/// What the model was told by every tool that answered, in order: the tool
/// results as the log holds them, which is what the next request is built from.
fn tool_results(s: &Session) -> Vec<String> {
    s.client()
        .events()
        .into_iter()
        .filter_map(|logged| match logged.event {
            SessionEvent::ToolResultLogged { content, .. } => Some(content),
            _ => None,
        })
        .collect()
}

/// A model asks which of several things the person wants, and the person
/// ticks two: both reach the model.
///
/// The screen used to draw every question as a single choice and send back one
/// pick — the tool promised "multiple", and the person could only ever give one.
#[tokio::test]
async fn a_multiple_choice_question_sends_back_every_answer_ticked() {
    let dir = scratch("ask-multiple");
    let script = replay(
        r#"{ text = "Asking.", calls = [ { name = "request_user_input", args = { header = "语言", question = "要支持哪些语言?", mode = "multiple", options = [ { label = "Python" }, { label = "Rust", description = "系统层" }, { label = "Go" } ] } } ] },
           { text = "Noted." }"#,
    );
    let s = start(tree(&dir, &script, &[REQUEST_USER_INPUT_LAYER])).await;
    let task = s.open().await;

    s.term.type_line("pick languages");
    until(&s, "space 勾选").await;
    // Read off the panel itself: the call's arguments are on screen too, in
    // the transcript, and would say the same words whether the panel did or not.
    let panel: String = s
        .term
        .last()
        .and_then(|f| f.part("ask").map(|p| p.lines.clone()))
        .expect("the question panel is up")
        .iter()
        .map(|l| l.plain() + "\n")
        .collect();
    assert!(panel.contains("要支持哪些语言"), "{panel}");
    assert!(
        panel.contains("系统层"),
        "what an answer means is shown:\n{panel}"
    );

    // Tick Python and Go with space, then walk to the row that sends them.
    s.term.press(KeyPress::ch(' '));
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::ch(' '));
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;

    let told = tool_results(&s).join("\n");
    assert!(
        told.contains("\"Python\"") && told.contains("\"Go\""),
        "both ticked answers reach the model: {told}"
    );
    assert!(
        !told.contains("Rust"),
        "and the unticked one does not: {told}"
    );
    assert!(s.screen().contains("Noted."), "the turn carried on");

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A model asks for words, and the person types them: the words reach the
/// model — not a "yes" or a "no" the screen made up because it had nothing
/// else to offer.
#[tokio::test]
async fn a_text_question_sends_back_what_the_person_typed() {
    let dir = scratch("ask-text");
    let script = replay(
        r#"{ text = "Asking.", calls = [ { name = "request_user_input", args = { header = "名字", question = "新仓库叫什么?", mode = "text" } } ] },
           { text = "Noted." }"#,
    );
    let s = start(tree(&dir, &script, &[REQUEST_USER_INPUT_LAYER])).await;
    let task = s.open().await;

    s.term.type_line("name it");
    until(&s, "新仓库叫什么").await;
    until(&s, "esc 拒绝").await;
    let panel = s.screen();
    assert!(
        !panel.contains("不了"),
        "no yes/no put in front of it:\n{panel}"
    );

    // Typed, and pasted: a paste while the question is up is the question's —
    // the composer is off screen, and words pasted into it would be lost.
    s.term.type_text("atomcode ");
    s.term.paste("lab 2");
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;

    let told = tool_results(&s).join("\n");
    assert!(
        told.contains("User answered: \"atomcode lab 2\""),
        "the model gets the words: {told}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A typo on the row of one's own words is fixed where it is: the arrows move
/// the caret back into the word, the missing letter goes in there, and the
/// model gets the word as corrected — not a line that had to be backspaced away.
#[tokio::test]
async fn a_typo_on_the_typing_row_is_fixed_where_it_is() {
    let dir = scratch("ask-caret");
    let script = replay(
        r#"{ text = "Asking.", calls = [ { name = "request_user_input", args = { header = "问候", question = "说什么?", mode = "single", options = [ { label = "hi" }, { label = "hey" } ] } } ] },
           { text = "Noted." }"#,
    );
    let s = start(tree(&dir, &script, &[REQUEST_USER_INPUT_LAYER])).await;
    let task = s.open().await;

    s.term.type_line("greet");
    until(&s, "说什么").await;
    // To the row of one's own words by its number, then the typo.
    s.term.press(KeyPress::ch('3'));
    s.term.type_text("helo");
    until(&s, "helo").await;
    s.term.press(KeyPress::plain(Key::Left));
    s.term.press(KeyPress::plain(Key::Left));
    s.term.press(KeyPress::ch('l'));
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;

    let told = tool_results(&s).join("\n");
    assert!(
        told.contains("User answered: \"hello\""),
        "the model gets the word as corrected: {told}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Two questions put together are answered on one panel — a page each and a
/// page to check them on — and go back as one reply, in order.
#[tokio::test]
async fn a_batch_of_questions_is_answered_on_one_panel_and_sent_together() {
    let dir = scratch("ask-batch");
    let script = replay(
        r#"{ text = "Asking.", calls = [ { name = "request_user_input", args = { questions = [ { header = "口味", question = "要哪个口味?", mode = "single", options = [ { label = "vanilla" }, { label = "pistachio" } ] }, { header = "名字", question = "新仓库叫什么?", mode = "text" } ] } } ] },
           { text = "Noted." }"#,
    );
    let s = start(tree(&dir, &script, &[REQUEST_USER_INPUT_LAYER])).await;
    let task = s.open().await;

    s.term.type_line("two things");
    until(&s, "切换题目").await;
    // Page one: the second answer, by its number. Page two: words.
    s.term.press(KeyPress::ch('2'));
    until(&s, "新仓库叫什么").await;
    s.term.type_line("lab");
    until(&s, "核对你的回答").await;
    let review = s.screen();
    assert!(
        review.contains("pistachio") && review.contains("lab"),
        "{review}"
    );
    s.term.press(KeyPress::ch('1'));
    s.quiet().await;

    let told = tool_results(&s).join("\n");
    assert!(
        told.contains("Q1 (口味): User selected: \"pistachio\"")
            && told.contains("Q2 (名字): User answered: \"lab\""),
        "both answers, in order: {told}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// ---- the command surface --------------------------------------------------

/// The window says which project this is, and what it is doing.
///
/// Four terminals open on four checkouts all say `atomcode` otherwise, and the
/// one thing a person needs from across the room is which is which. The
/// directory is the fallback; a session that has been named says its name.
#[tokio::test]
async fn the_window_says_which_project_this_is() {
    let dir = scratch("title");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;
    // The screen's own directory, which is what it puts in the status line —
    // the agent's root is a separate thing and in this harness they differ.
    let cwd = std::env::current_dir().expect("a working directory");
    let here = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .expect("a directory name");
    // An idle, not-yet-named session: the green status dot, then the project the
    // session is working in (its fallback name).
    assert_eq!(
        s.term.title().as_deref(),
        Some(format!("🟢 {here}").as_str()),
        "the window is a status dot then where the session is working"
    );
    task.abort();
}

/// The light goes out when the person says so, and that is the whole setting.
///
/// The other half of the same judgement: `the_window_says_which_project_this_is`
/// is the on state, and this is the off one reading the configuration the
/// launcher hands over — `ui.terminal_status_glyph = false` leaves the title
/// exactly as it was before the light existed.
#[tokio::test]
async fn the_window_can_be_named_without_the_light() {
    let dir = scratch("title-off");
    let s = start_with_connection_and_settings(
        tree(&dir, &replay(r#"{ text = "ok" }"#), &[]),
        Some(Arc::new(OneSetting(
            atomcode_tui::settings::STATUS_DOT,
            "false",
        ))),
    )
    .await;
    let task = s.open().await;
    s.quiet().await;
    let cwd = std::env::current_dir().expect("a working directory");
    let here = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .expect("a directory name");
    assert_eq!(
        s.term.title().as_deref(),
        Some(here),
        "turned off is the plain name, byte for byte"
    );
    task.abort();
}

/// And the light follows what the session is doing, which is the only reason it
/// exists: a tab strip that says "busy" and "wants you" without being read.
#[tokio::test]
async fn the_light_turns_green_yellow_and_red() {
    let dir = scratch("light");
    // A turn that stays in flight long enough to be looked at, then a risky call
    // that stops and waits for a person — the two states that are not idle.
    let script = replay(
        r#"{ text = "Reading.", calls = [ { name = "bash", args = { command = "sleep 0.5" } } ] },
           { text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "written" } } ] }"#,
    );
    let s = start(asking(&dir, &script)).await;
    let task = s.open().await;
    s.quiet().await;
    assert!(
        s.term.title().is_some_and(|t| t.starts_with('🟢')),
        "idle before anything is asked of it: {:?}",
        s.term.title()
    );

    s.term.type_line("go");
    until(&s, "正在运行 1 个工具").await;
    assert!(
        s.term.title().is_some_and(|t| t.starts_with('🟡')),
        "a turn in flight is a yellow light: {:?}",
        s.term.title()
    );

    // The risky call stops the turn on a question, and that is the state a
    // person is actually wanted in.
    until(&s, "esc 拒绝").await;
    assert!(
        s.term.title().is_some_and(|t| t.starts_with('🔴')),
        "a question waiting is a red light: {:?}",
        s.term.title()
    );

    s.term.press(KeyPress::ch('1'));
    s.quiet().await;
    assert!(
        s.term.title().is_some_and(|t| t.starts_with('🟢')),
        "and back to idle once it is answered: {:?}",
        s.term.title()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn typing_a_slash_shows_what_is_available_and_narrows_as_you_type() {
    let dir = scratch("menu");
    // Without the welcome block: it names commands among its quick-start tips,
    // and this test proves the menu narrowed by looking for one being gone from
    // the screen. Two rows naming the same command is not this test's subject —
    // the welcome has its own (`a_new_session_opens_with_the_welcome_…`).
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/");
    s.quiet().await;
    let all = s.screen();
    // Two that are alphabetically near the top, because the menu shows the
    // first ten of what matches and this one is about opening and narrowing,
    // not about which commands the build happens to ship.
    assert!(all.contains("/clear"), "the menu opens:\n{all}");
    assert!(all.contains("/compact"), "{all}");

    s.term.type_text("comp");
    s.quiet().await;
    let narrowed = s.screen();
    assert!(narrowed.contains("/compact"), "{narrowed}");
    assert!(!narrowed.contains("/clear"), "it narrows:\n{narrowed}");

    // And it closes again when the slash goes away.
    for _ in 0..5 {
        s.term.press(KeyPress::plain(Key::Backspace));
    }
    s.quiet().await;
    assert!(
        !s.screen().contains("/compact"),
        "menu closed:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Which row of the slash menu is the lit one, read off the drawn frame.
///
/// Read off the part's own lines rather than re-derived: the claim under test
/// is "the panel that is on screen lights this row", and a row computed some
/// other way would pass even if the panel painted a different one.
fn lit_slash_row(s: &Session) -> Option<u16> {
    let bright = Some(atomcode_tui::Color::role(
        atomcode_tui::theme::Role::PanelSelBg,
    ));
    let part = s.term.last()?.part("menu")?.clone();
    (0..part.lines.len())
        .find(|i| part.lines[*i].spans[0].style.bg == bright)
        .map(|i| part.rect.y + i as u16)
}

#[tokio::test]
async fn a_slash_menu_opens_with_its_first_row_lit_and_the_arrows_walk_it() {
    // The requirement, through the whole machine: type a slash and something is
    // already pointed at, so a return does the obvious thing without an arrow
    // press first. Then down/up move the highlight without moving the panel.
    let dir = scratch("menu-lit");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/");
    // Wait for the menu itself, not for a particular command to be in it: which
    // names land in the first window is the command table's business and moves
    // whenever one is added.
    for _ in 0..400 {
        if s.term.last().map(|f| f.part("menu").is_some()) == Some(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let panel = s.term.last().unwrap().part("menu").expect("open").rect;
    assert_eq!(
        lit_slash_row(&s),
        Some(panel.y),
        "the first row is lit the moment the menu opens:\n{}",
        s.screen()
    );

    // Down walks the highlight one row at a time, and the panel stays put.
    s.term.press(KeyPress::plain(Key::Down));
    s.quiet().await;
    assert_eq!(
        lit_slash_row(&s),
        Some(panel.y + 1),
        "down lit the next row"
    );
    assert_eq!(
        s.term.last().unwrap().part("menu").unwrap().rect,
        panel,
        "and the panel moved with the cursor"
    );

    // Up comes back, and stops at the top rather than wrapping.
    s.term.press(KeyPress::plain(Key::Up));
    s.quiet().await;
    s.term.press(KeyPress::plain(Key::Up));
    s.quiet().await;
    assert_eq!(
        lit_slash_row(&s),
        Some(panel.y),
        "up came back to the first"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn enter_runs_the_lit_command_without_its_name_being_typed_out() {
    // The other half of the highlight: once something has been named, one
    // keystroke runs it. The name never has to be typed in full, which is what
    // a list with a lit row is for.
    let dir = scratch("menu-run");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/cont");
    until(&s, "/context").await;
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    assert!(
        s.screen().contains("条事实"),
        "enter ran the lit command:\n{}",
        s.screen()
    );
    // And the prefix did not stay behind on the line.
    assert!(
        !s.screen().contains("/cont "),
        "the prefix outlived the command that ran:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The lit row of the slash menu, as it was drawn.
///
/// The name is read off the frame rather than assumed, so a claim about "the
/// row that is lit" does not quietly become a claim about the sort order.
fn lit_slash_name(s: &Session) -> Option<String> {
    let bright = Some(atomcode_tui::Color::role(
        atomcode_tui::theme::Role::PanelSelBg,
    ));
    let part = s.term.last()?.part("menu")?.clone();
    let row = (0..part.lines.len()).find(|i| part.lines[*i].spans[0].style.bg == bright)?;
    part.lines[row]
        .plain()
        .trim()
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .map(str::to_string)
}

#[tokio::test]
async fn enter_takes_the_lit_row_even_before_a_name_is_typed() {
    // The key belongs to the list whenever the list is up. It is **taken**, not
    // swallowed: a menu that keeps the return key and then does nothing with it
    // is a dead key, and the person pressing it cannot tell that from a freeze.
    //
    // Nothing is special-cased about a bare `/`. The lit row is on screen and
    // says what it would do, and that is the contract every list here keeps —
    // the question panel's words for it are "a stray return takes what the
    // screen shows it would take, never a hidden default".
    let dir = scratch("menu-bare-slash");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/");
    until(&s, "/cancel-all").await;
    let lit = lit_slash_name(&s).expect("a lit row");
    assert!(
        !lit.is_empty(),
        "the list is up with a lit row:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    assert!(
        s.term.last().unwrap().part("menu").is_none(),
        "enter was swallowed — the list is still up:\n{}",
        s.screen()
    );
    assert!(
        !s.screen().contains("❯ /"),
        "the line still holds the slash, so nothing was taken:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn the_arrows_choose_the_row_that_enter_then_takes() {
    // What a highlight is *for*: the row the arrows walked to is the row the
    // return key acts on. Two matches, so "the second one" is a real choice and
    // not the first row by another name.
    //
    // The prefix is `/con` and not `/co`, and that is load-bearing: `/co` also
    // reaches `/compact`, so the row one arrow away is whatever the third name
    // sorts in as — which moved the day `/config` was added. A criterion about
    // "the arrow moved it" must not double as a criterion about how many
    // commands happen to start with two letters.
    let dir = scratch("menu-arrows-take");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/con");
    until(&s, "/config").await;
    assert_eq!(lit_slash_name(&s).as_deref(), Some("config"));

    s.term.press(KeyPress::plain(Key::Down));
    s.quiet().await;
    assert_eq!(
        lit_slash_name(&s).as_deref(),
        Some("context"),
        "the arrow moved the highlight:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    assert!(
        s.screen().contains("条事实"),
        "enter took the row the arrows chose, not the first one:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn tab_completes_the_lit_command_onto_the_line() {
    // Tab *completes*: the lit name goes onto the line so its argument can be
    // typed. It does not run — that is enter's job, and the two are different
    // for exactly this reason.
    let dir = scratch("menu-complete");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/comp");
    until(&s, "/compact").await;
    s.term.press(KeyPress::plain(Key::Tab));
    s.quiet().await;

    // The command is now what is typed, and the menu has narrowed to it — the
    // point being that the line holds the whole name rather than the prefix.
    assert!(
        s.screen().contains("/compact"),
        "the name was put on the line:\n{}",
        s.screen()
    );
    // And it did not run: nothing has been compacted.
    assert!(
        !s.screen().contains("已压缩") && !s.screen().contains("暂时没有值得压缩的"),
        "tab ran the command instead of completing it:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn enter_takes_the_lit_row_and_a_command_that_wants_an_argument_asks() {
    // The return key belongs to the list while the list is up: the row that is
    // lit is the row that is taken. What "taken" means is the command's own
    // business — `/effort` has a closed set of levels, so taking its row opens
    // that set inline (one row per level) rather than running bare, and the pick
    // a person came for is made a level down.
    let dir = scratch("menu-enter");
    let s = start(tree(
        &dir,
        &replay_effort(r#"{ text = "ok" }"#, ALL_EFFORT_LEVELS),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    // Half a name, a lit row, and one keystroke: the levels open inline.
    s.term.type_text("/effo");
    until(&s, "/effort").await;
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "effort high").await;
    let screen = s.screen();
    assert!(
        s.term.last().unwrap().part("menu").is_some(),
        "enter opened the level list inline:\n{screen}"
    );
    assert!(
        screen.contains("effort high") || screen.contains("effort medium"),
        "the list shows the levels one per row:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn tab_completes_a_command_that_takes_an_argument_and_leaves_a_space() {
    // What the registry is asked for and the label only shows: a name completed
    // without the space that says "something goes here" leaves the caret in the
    // wrong place, and the person has to type the separator the menu knew about.
    // `/mode` takes a free word — a mode name — so tab leaves the space;
    // `/effort`'s closed set does not, opening its values inline instead.
    let dir = scratch("menu-takes");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    // `mode` sorts before `model`, so it is the lit row the moment both match.
    s.term.type_text("/mode");
    until(&s, "/mode").await;
    s.term.press(KeyPress::plain(Key::Tab));
    s.quiet().await;
    assert!(
        !s.screen().contains("现在是"),
        "tab ran a command that wants an argument:\n{}",
        s.screen()
    );

    // Which leaves the name and a space on the line: typing the argument reads
    // as `/mode plan`, not `/modeplan`, so it lands apart from the name and can
    // be sent the ordinary way.
    s.term.type_text("plan");
    s.quiet().await;
    assert!(
        s.screen().contains("/mode plan"),
        "tab left the separating space the closed-set command does not:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A command whose free-text argument is required and has no bare form — like
/// `/rename` — completes onto the line when its row is taken, rather than
/// dispatching a bare "needs a name". So the return key leaves it in the
/// composer for the argument to be typed, the same as a closed set opens its
/// values.
#[tokio::test]
async fn enter_on_a_required_argument_command_completes_it_rather_than_running() {
    let dir = scratch("menu-require-arg");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/ren");
    until(&s, "/rename").await;
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;

    // Not dispatched: the name and a space are on the line, so typing the
    // argument reads as `/rename my work` and lands apart from the name. Had
    // enter run it bare, the line would be empty behind a "needs a name" reply
    // and this text would stand alone.
    s.term.type_text("my work");
    s.quiet().await;
    assert!(
        s.screen().contains("/rename my work"),
        "enter completed the name onto the line for the argument to be typed:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn the_pointer_lights_and_chooses_a_slash_menu_row() {
    // A menu only the keyboard can drive is a menu half the people who reach for
    // the mouse cannot use. The row the pointer is over is the row that is lit,
    // and the row a press lands on is the row that was drawn there.
    let dir = scratch("menu-mouse");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/re");
    until(&s, "/resume").await;
    let panel = s.term.last().unwrap().part("menu").expect("open").rect;
    assert_eq!(lit_slash_row(&s), Some(panel.y));

    // Hover the row `/resume` is on — **found in the drawn frame**, not assumed
    // to be a particular index. A criterion that hardcoded "the third row"
    // breaks every time a command is added or removed, and what it would be
    // reporting then is the sort order, not the pointer.
    let menu = s.term.last().unwrap().part("menu").unwrap().lines.clone();
    let row = menu
        .iter()
        .position(|l| l.plain().trim().starts_with("/resume"))
        .expect("`/resume` is in the menu after typing `/re`") as u16;
    s.term.pointer(
        atomcode_tui::surface::Click::Hover,
        panel.x + 3,
        panel.y + row,
    );
    s.quiet().await;
    assert_eq!(
        lit_slash_row(&s),
        Some(panel.y + row),
        "the pointer lit the row it is over:\n{}",
        s.screen()
    );
    let drawn = menu[row as usize].plain();
    let name = drawn
        .trim()
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .expect("a command name")
        .to_string();
    assert_eq!(name, "resume", "the row under the pointer: {drawn:?}");

    // And a press there takes *that* row. `/resume` wants an argument and has no
    // closed set to offer, so taking it dispatches the bare command, which opens
    // the session picker — the row under the pointer, not the first row.
    s.term.pointer(
        atomcode_tui::surface::Click::Press,
        panel.x + 3,
        panel.y + row,
    );
    s.term.pointer(
        atomcode_tui::surface::Click::Release,
        panel.x + 3,
        panel.y + row,
    );
    s.quiet().await;
    assert!(
        s.term.last().unwrap().part("resume").is_some()
            || s.screen().contains("没有别的存下的会话"),
        "the press did not take the row it landed on:\n{}",
        s.screen()
    );
    assert!(
        s.term.last().unwrap().part("menu").is_none(),
        "and taking it put the list away:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn esc_puts_the_slash_menu_away_without_losing_the_line() {
    // Esc is "not that list, this line". It is not clear-the-line: the slash is
    // still there, and the menu stays away until something changes what is
    // typed.
    let dir = scratch("menu-esc-slash");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/comp");
    until(&s, "/compact").await;
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    assert!(
        s.term.last().unwrap().part("menu").is_none(),
        "the menu is still up:\n{}",
        s.screen()
    );
    assert!(
        s.screen().contains("/comp"),
        "esc cleared the line instead of the list:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// ---- right-click on the composer ----------------------------------------

/// The surface is the only thing that can put a pointer event in, so a test
/// reaches the menu the way a hand does: through the recorder.
fn right_click(s: &Session, x: u16, y: u16) {
    s.term
        .pointer(atomcode_tui::surface::Click::RightPress, x, y);
}

/// Right-click on the composer, where its four items live.
///
/// The field's rect is read off the last frame rather than guessed at a row: the
/// composer's position depends on what is above it, and where the press lands
/// now decides what the menu offers — a press over the conversation is not the
/// composer's menu.
fn right_click_composer(s: &Session) {
    let field = s
        .term
        .last()
        .expect("a frame")
        .part("input")
        .expect("the composer")
        .rect;
    right_click(s, field.x + 2, field.y);
}

#[tokio::test]
async fn right_click_on_the_composer_opens_a_menu_that_does_what_it_says() {
    // The path a person takes: type something, right-click, pick "复制全文",
    // and find the words on the clipboard. Nothing here is a unit test of the
    // menu — it went in as a right button and came out as a clipboard write.
    let dir = scratch("right-click");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("hello composer");
    s.quiet().await;
    assert!(
        !s.screen().contains("复制全文"),
        "no menu until it is asked for:\n{}",
        s.screen()
    );

    // On the composer's own row, which is where the field is.
    let field = s
        .term
        .last()
        .expect("a frame")
        .part("input")
        .expect("the composer")
        .rect;
    right_click(&s, field.x + 4, field.y);
    s.quiet().await;

    let screen = s.screen();
    assert!(screen.contains("复制全文"), "the menu opened:\n{screen}");
    assert!(screen.contains("粘贴") && screen.contains("清空") && screen.contains("发送"));
    assert!(
        s.term.last().unwrap().part("context-menu").is_some(),
        "it is on screen as a panel of its own"
    );

    // The first item, chosen with the keyboard the way a menu is used.
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    assert_eq!(
        s.term.clipboard_text().as_deref(),
        Some("hello composer"),
        "复制全文 put the composer on the clipboard"
    );
    assert!(
        !s.screen().contains("复制全文"),
        "picking closed the menu:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn the_menu_s_paste_reads_the_clipboard_into_what_is_being_typed() {
    let dir = scratch("menu-paste");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("tail");
    s.term.set_clipboard_text("head ");
    // Home first, so the paste has somewhere to land other than the end: what
    // is under test is "at the caret", and a paste that only ever appends is
    // the case that would pass by accident.
    s.term.press(KeyPress::plain(Key::Home));
    s.quiet().await;
    right_click_composer(&s);
    s.quiet().await;

    // Down to 粘贴, and pick it.
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    assert!(
        s.screen().contains("head tail"),
        "the clipboard went in at the caret:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_pointer_chooses_the_row_the_words_were_drawn_on() {
    // The menu is opened by the secondary button, so the primary button has to
    // be able to use it: a menu you can only drive from the keyboard is a menu
    // half the people who reach for the mouse cannot use. And it has to choose
    // the row that was *clicked* — not the row the pointer's own geometry would
    // have been under had the menu not slid up to fit the screen.
    let dir = scratch("menu-click");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("tail");
    s.term.set_clipboard_text("head ");
    s.term.press(KeyPress::plain(Key::Home));
    s.quiet().await;
    right_click_composer(&s);
    s.quiet().await;

    let menu = s
        .term
        .last()
        .expect("a frame")
        .part("context-menu")
        .expect("the menu opened")
        .rect;
    // The row "粘贴" was drawn on, read off the part rather than re-derived.
    let paste = menu.y + 1;
    s.term
        .pointer(atomcode_tui::surface::Click::Press, menu.x + 2, paste);
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("head tail"),
        "the click on that row ran that row:\n{screen}"
    );
    assert!(
        !screen.contains("复制全文"),
        "and choosing closed the menu:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn the_menu_s_send_hands_the_line_to_the_model() {
    // "发送" is not a second way to send: it is the same action the Enter key
    // resolves to, which is the whole reason the menu speaks `Action`.
    let dir = scratch("menu-send");
    let s = start(tree(&dir, &replay(r#"{ text = "heard you" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("say it through the menu");
    s.quiet().await;
    right_click_composer(&s);
    s.quiet().await;
    for _ in 0..3 {
        s.term.press(KeyPress::plain(Key::Down));
    }
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("heard you"),
        "the model was asked:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_click_away_puts_the_menu_away_and_still_does_its_own_job() {
    let dir = scratch("menu-dismiss");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("keep me");
    s.quiet().await;
    right_click_composer(&s);
    s.quiet().await;
    assert!(s.screen().contains("复制全文"), "{}", s.screen());

    // Somewhere else entirely — the far corner of the conversation.
    s.term.pointer(atomcode_tui::surface::Click::Press, 1, 1);
    s.term.pointer(atomcode_tui::surface::Click::Release, 1, 1);
    s.quiet().await;

    let screen = s.screen();
    assert!(
        !screen.contains("复制全文"),
        "the press put it away:\n{screen}"
    );
    assert!(
        screen.contains("keep me"),
        "and the draft is untouched — dismissing is not clearing:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn esc_puts_the_menu_away_without_picking_anything() {
    let dir = scratch("menu-esc");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("untouched");
    s.quiet().await;
    right_click_composer(&s);
    s.quiet().await;
    assert!(s.screen().contains("复制全文"));

    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    let screen = s.screen();
    assert!(!screen.contains("复制全文"), "{screen}");
    assert!(
        screen.contains("untouched"),
        "esc closed the menu, not the draft:\n{screen}"
    );
    assert_eq!(
        s.term.clipboard_text(),
        None,
        "nothing was copied on the way out"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn moving_the_pointer_over_a_row_makes_it_the_highlighted_one() {
    // The pointer says which row it means before it clicks. Read off the drawn
    // part — the row whose background is the brighter panel — because "the menu
    // tracks the pointer" and "the menu paints the pointer's row" are two
    // different claims and only the second one is the feature.
    //
    // The screen row matters, not the index within the part: read off the index
    // and any hover that repaints the same shape passes, including one that
    // slid the whole panel a row down the screen under the pointer.
    let dir = scratch("menu-hover");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("tail");
    s.term.set_clipboard_text("head ");
    s.term.press(KeyPress::plain(Key::Home));
    s.quiet().await;
    right_click_composer(&s);
    s.quiet().await;

    let menu = s
        .term
        .last()
        .expect("a frame")
        .part("context-menu")
        .expect("the menu opened")
        .rect;
    let bright = Some(atomcode_tui::Color::role(
        atomcode_tui::theme::Role::PanelSelBg,
    ));
    let lit_row = |s: &Session| -> Option<u16> {
        let part = s
            .term
            .last()
            .expect("a frame")
            .part("context-menu")?
            .clone();
        (0..part.lines.len())
            .find(|i| part.lines[*i].spans[0].style.bg == bright)
            .map(|i| part.rect.y + i as u16)
    };

    assert_eq!(
        lit_row(&s),
        Some(menu.y),
        "the menu opens pointing at its first row"
    );

    // Onto "粘贴", one row down.
    s.term
        .pointer(atomcode_tui::surface::Click::Hover, menu.x + 2, menu.y + 1);
    s.quiet().await;
    assert_eq!(
        lit_row(&s),
        Some(menu.y + 1),
        "the row the pointer is over is not the row drawn brighter:\n{}",
        s.screen()
    );

    // And the row that is lit is the row a key would take: a highlight over one
    // row while Enter takes another is the bug this is about.
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    let screen = s.screen();
    assert!(
        screen.contains("head tail"),
        "enter did not take the row the pointer was over:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_drag_selection_confirms_the_copy_right_away() {
    // Dragging to select auto-copies; the confirmation belongs on that gesture,
    // not only after a second, explicit copy through the menu or a chord.
    let dir = scratch("drag-copy-hint");
    let s = start(tree(&dir, &replay(r#"{ text = "the model spoke" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_line("ask a question");
    s.quiet().await;

    let row = s
        .screen()
        .lines()
        .position(|l| l.contains("spoke"))
        .expect("the answer is on screen") as u16;
    let stream = s
        .term
        .last()
        .expect("a frame")
        .part("stream")
        .expect("the conversation")
        .rect;
    s.term
        .pointer(atomcode_tui::surface::Click::Press, stream.x, row);
    s.term
        .pointer(atomcode_tui::surface::Click::Drag, stream.right() - 1, row);
    s.term.pointer(
        atomcode_tui::surface::Click::Release,
        stream.right() - 1,
        row,
    );
    s.quiet().await;

    assert!(
        s.term.clipboard_text().is_some_and(|t| t.contains("spoke")),
        "the drag copied the answer"
    );
    assert!(
        s.screen().contains("已复制选中"),
        "the drag-copy confirms itself on screen right away:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn right_click_over_a_selection_copies_the_selection_and_not_the_field() {
    // A right-click usually lands on text that was just selected, and the menu
    // it opens is about *that*. The copy item sent the composer's contents
    // instead, which over a selection is a different buffer entirely — and an
    // empty one, which is why the answer used to be "没有可复制的内容".
    let dir = scratch("menu-copy-selection");
    let s = start(tree(&dir, &replay(r#"{ text = "the model spoke" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_line("ask a question");
    s.quiet().await;

    // A draft left in the field, so "it copied the field" has a way to show.
    s.term.type_text("draft in the field");
    s.quiet().await;

    // Select a run of the answer by dragging across the row it is drawn on.
    let row = s
        .screen()
        .lines()
        .position(|l| l.contains("spoke"))
        .expect("the answer is on screen") as u16;
    let stream = s
        .term
        .last()
        .expect("a frame")
        .part("stream")
        .expect("the conversation")
        .rect;
    s.term
        .pointer(atomcode_tui::surface::Click::Press, stream.x, row);
    s.term
        .pointer(atomcode_tui::surface::Click::Drag, stream.right() - 1, row);
    s.term.pointer(
        atomcode_tui::surface::Click::Release,
        stream.right() - 1,
        row,
    );
    s.quiet().await;

    let taken = s
        .term
        .clipboard_text()
        .expect("the drag copied what it covered");
    assert!(
        taken.contains("spoke"),
        "the drag selected the answer: {taken:?}"
    );

    // Now the menu, opened on the selection. Wipe the clipboard first, so a
    // menu that copies nothing at all cannot pass by leaving the drag's text.
    right_click(&s, stream.x + 2, row);
    s.quiet().await;
    let screen = s.screen();
    assert!(
        screen.contains("复制选中"),
        "the menu says what it will copy:\n{screen}"
    );
    s.term.set_clipboard_text("<untouched>");
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;

    assert_eq!(
        s.term.clipboard_text().as_deref(),
        Some(taken.as_str()),
        "the menu copied the selection"
    );
    assert_ne!(
        s.term.clipboard_text().as_deref(),
        Some("draft in the field"),
        "the menu copied the field over a selection"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn copying_says_so_on_the_tip_row_and_not_in_the_conversation() {
    // A copy is true of *now*: it is not an event in the conversation, and a
    // block for it pushed every row of the conversation up one to make room for
    // a sentence nobody reads twice. It belongs on the row that was reserved for
    // exactly this — and it has to go away by itself, because a tip that had to
    // be cleared would be worse than no tip.
    let dir = scratch("menu-copy-tip");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("hello composer");
    s.quiet().await;
    let parted = s.term.last().expect("a frame");
    let field = parted.part("input").expect("the field").rect;
    let words = parted.part("stream").expect("the words").lines.len();

    right_click(&s, field.x + 4, field.y);
    s.quiet().await;
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;

    assert_eq!(
        s.term.clipboard_text().as_deref(),
        Some("hello composer"),
        "the copy happened"
    );

    let frame = s.term.last().expect("a frame");
    let tip: String = frame
        .part("tip")
        .expect("the reserved row")
        .lines
        .iter()
        .map(|l| l.plain())
        .collect();
    assert!(
        tip.contains("已复制到剪贴板"),
        "the tip row says so: {tip:?}"
    );
    let conversation: String = frame
        .part("stream")
        .expect("the words")
        .lines
        .iter()
        .map(|l| l.plain())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !conversation.contains("已复制"),
        "and the conversation is not told:\n{conversation}"
    );
    assert_eq!(
        frame.part("stream").expect("the words").lines.len(),
        words,
        "the conversation did not grow a row for it:\n{conversation}"
    );

    // Three seconds on, nobody having asked: gone. The frame after it paints the
    // same blank row, so the field is where it was.
    tokio::time::sleep(Duration::from_millis(3_200)).await;
    s.quiet().await;
    let later = s.term.last().expect("a frame");
    let tip: String = later
        .part("tip")
        .expect("the row is still reserved")
        .lines
        .iter()
        .map(|l| l.plain())
        .collect();
    assert!(
        tip.trim().is_empty(),
        "the tip went away by itself: {tip:?}"
    );
    assert_eq!(
        later.part("input").expect("the field").rect,
        field,
        "and the box did not move when it did"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_right_click_outside_the_composer_offers_only_what_belongs_there() {
    // 清空 and 发送 act on what is being typed, so a press over the conversation
    // must not offer them: they belong to a box the pointer is not in. What is
    // left is what is still true of the press — the text it landed on.
    let dir = scratch("menu-outside");
    let s = start(tree(&dir, &replay(r#"{ text = "the model spoke" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_line("ask a question");
    s.quiet().await;

    // In the conversation, with nothing selected: nothing a press here can ask
    // for, so nothing is offered.
    let stream = s
        .term
        .last()
        .expect("a frame")
        .part("stream")
        .expect("the words")
        .rect;
    right_click(&s, stream.x + 2, stream.y);
    s.quiet().await;
    let screen = s.screen();
    assert!(
        !screen.contains("清空") && !screen.contains("发送") && !screen.contains("粘贴"),
        "no composer verbs outside the composer:\n{screen}"
    );
    assert!(
        !screen.contains("复制全文"),
        "and not the field's copy either — the field is not what was pressed:\n{screen}"
    );

    // With something selected, the menu is about those words: copy them, and
    // nothing that would act on the composer.
    let row = s
        .screen()
        .lines()
        .position(|l| l.contains("spoke"))
        .expect("the answer is on screen") as u16;
    s.term
        .pointer(atomcode_tui::surface::Click::Press, stream.x, row);
    s.term
        .pointer(atomcode_tui::surface::Click::Drag, stream.right() - 1, row);
    s.term.pointer(
        atomcode_tui::surface::Click::Release,
        stream.right() - 1,
        row,
    );
    s.quiet().await;
    right_click(&s, stream.x + 2, row);
    s.quiet().await;
    let screen = s.screen();
    assert!(
        screen.contains("复制选中"),
        "the menu copies what is selected:\n{screen}"
    );
    assert!(
        !screen.contains("清空") && !screen.contains("发送") && !screen.contains("粘贴"),
        "and offers nothing that belongs to the composer:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn the_menu_over_a_selection_keeps_its_own_colours() {
    // The menu is a panel raised over the screen, so what it covers it covers.
    // It did not: the selection was highlighted after the menu was drawn, so a
    // menu opened on selected text came out striped with the selection running
    // through its rows.
    let dir = scratch("menu-over-selection");
    let s = start(tree(&dir, &replay(r#"{ text = "the model spoke" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_line("ask a question");
    s.quiet().await;

    let row = s
        .screen()
        .lines()
        .position(|l| l.contains("spoke"))
        .expect("the answer is on screen") as u16;
    let stream = s
        .term
        .last()
        .expect("a frame")
        .part("stream")
        .expect("the conversation")
        .rect;
    s.term
        .pointer(atomcode_tui::surface::Click::Press, stream.x, row);
    s.term
        .pointer(atomcode_tui::surface::Click::Drag, stream.right() - 1, row);
    s.term.pointer(
        atomcode_tui::surface::Click::Release,
        stream.right() - 1,
        row,
    );
    s.quiet().await;

    // Open the menu on the selected row, so the two have to share cells.
    right_click(&s, stream.x + 2, row);
    s.quiet().await;

    let part = s
        .term
        .last()
        .expect("a frame")
        .part("context-menu")
        .expect("the menu opened")
        .clone();
    assert!(
        part.rect.contains(stream.x + 2, row),
        "the menu has to cover the row the selection is on, or this proves \
         nothing: menu {:?}, selected row {row}",
        part.rect
    );
    for (i, line) in part.lines.iter().enumerate() {
        assert!(
            line.spans.iter().all(|s| !s.style.reverse),
            "menu row {i} was recoloured by the selection under it: {line:?}"
        );
    }

    s.term.press(KeyPress::plain(Key::Esc));
    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn the_menu_asks_the_terminal_for_the_pointer_only_while_it_is_open() {
    // The hover tests above feed `Click::Hover` straight into the recorder, so
    // they pass whether or not a real terminal would ever have sent one. It
    // would not: a plain hover is DECSET 1003, and the screen asks for it only
    // for as long as something follows the pointer. This is the assertion that
    // the request goes out — the regression being a menu that lights the row
    // under the pointer on a machine where the pointer's row never arrives.
    let dir = scratch("menu-motion");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.quiet().await;
    assert!(
        !s.term.motion(),
        "the screen asks for free motion with nothing following the pointer"
    );

    right_click_composer(&s);
    s.quiet().await;
    assert!(s.screen().contains("复制全文"), "the menu opened");
    assert!(
        s.term.motion(),
        "the menu follows the pointer, so the terminal has to be reporting it"
    );

    // And it is handed back when the menu goes away — a terminal left in 1003
    // sends an event for every cell the pointer crosses for the rest of the
    // session, for nothing.
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    assert!(
        !s.screen().contains("复制全文"),
        "esc closed the menu:\n{}",
        s.screen()
    );
    assert!(
        !s.term.motion(),
        "the menu closed but the terminal is still reporting every cell"
    );

    // …and what is handed back is the *hover*, not the pointer. One tracker,
    // three mutually exclusive settings: the `1003l` that stops free motion
    // stops the clicks with it, so closing the menu has to leave the terminal in
    // button reporting. Asserted as the state rather than as the bytes, because
    // the bug was that the two were thought to be the same thing — the old pair
    // of flags said "no hover" and could not say "and still no buttons", so the
    // pointer was gone for the rest of the session and `ctrl-o` needed two
    // presses to return it: the first turned off what was already off.
    assert_eq!(
        s.term.pointer_mode(),
        atomcode_tui::ansi::Pointer::Buttons,
        "closing the menu gave the clicks away with the hover"
    );
    // The bytes agree with the state, which is what a real terminal reads. Named
    // as the constant rather than through `Pointer::escape` on purpose: asking
    // the function under test what it produced is not an oracle, and this
    // assertion is the one that notices the escape going back to a bare `1003l`.
    assert_eq!(
        s.term.escapes().last().map(String::as_str),
        Some(atomcode_tui::ansi::MOUSE_ON),
        "and that is what went out on the wire"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn moving_the_pointer_across_the_menu_chooses_nothing() {
    // A move is not a press. If it were routed as one, sliding across the menu
    // on the way to somewhere else would pick a row and close the panel —
    // "清空" under a pointer that never clicked.
    let dir = scratch("menu-hover-past");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("keep me");
    s.quiet().await;
    right_click_composer(&s);
    s.quiet().await;

    let menu = s
        .term
        .last()
        .expect("a frame")
        .part("context-menu")
        .expect("the menu opened")
        .rect;
    // Straight across every row, then off the right edge.
    for dy in 0..menu.h {
        s.term
            .pointer(atomcode_tui::surface::Click::Hover, menu.x + 2, menu.y + dy);
        s.quiet().await;
    }
    s.term.pointer(
        atomcode_tui::surface::Click::Hover,
        menu.right() + 5,
        menu.y,
    );
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("复制全文"),
        "a move over the menu chose something and closed it:\n{screen}"
    );

    // That the draft survived is proven once the menu is out of the way: the
    // panel is anchored at the cell it was asked for, and over the composer that
    // is the composer's own rows, so "keep me" is under it rather than gone.
    // Reading it off the screen with the menu up would test the overlap.
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    assert!(
        s.screen().contains("keep me"),
        "a move cleared the draft:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_command_answers_on_screen_and_never_reaches_the_model() {
    let dir = scratch("cmd");
    let s = start(tree(&dir, &replay(r#"{ text = "the model spoke" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_line("/context");
    s.quiet().await;
    let screen = s.screen();
    assert!(
        screen.contains("条事实"),
        "the answer is on screen:\n{screen}"
    );
    assert!(
        !screen.contains("the model spoke"),
        "a command must not start a turn:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn an_unknown_command_suggests_instead_of_vanishing() {
    let dir = scratch("typo");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    // The menu has to be out of the way first. With it open, enter takes the lit
    // row — that is the whole point of the highlight — so the dispatcher's
    // suggestion is the path a half-typed name takes when the list is not
    // standing in front of it. Esc is the one keystroke that says so, and it
    // leaves the line alone.
    s.term.type_text("/comp");
    until(&s, "/compact").await;
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    let screen = s.screen();
    assert!(screen.contains("/compact"), "it suggests:\n{screen}");

    // `/wat` matches nothing, so no menu opens at all and enter goes straight to
    // the dispatcher.
    s.term.type_line("/wat");
    s.quiet().await;
    assert!(
        s.screen().contains("/help"),
        "and points at help:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_command_and_a_key_share_one_implementation() {
    let dir = scratch("shared");
    std::fs::write(dir.join("a.rs"), "x").unwrap();
    let script = replay(
        r#"{ text = "Reading.", calls = [ { name = "read_file", args = { file_path = "a.rs" } } ] },
           { text = "done" }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;
    s.term.type_line("hi");
    s.quiet().await;

    // Step the cycle once with the key, then step it once with the command from
    // the same starting point. If they were two implementations the two screens
    // would differ. The cycle is four states (`full → head → each → group →
    // full`), so after the key takes its one step we run the rest of the way
    // round to `full` before the command takes its one step.
    s.term.press(KeyPress::ctrl('t')); // full -> head
    s.quiet().await;
    let by_key = s.screen();

    s.term.press(KeyPress::ctrl('t')); // head -> each
    s.quiet().await;
    s.term.press(KeyPress::ctrl('t')); // each -> group
    s.quiet().await;
    s.term.press(KeyPress::ctrl('t')); // group -> full, back to the start
    s.quiet().await;
    s.term.type_line("/tools");
    s.quiet().await;
    let by_command = s.screen();

    let stream_of = |screen: &str| {
        screen
            .lines()
            .filter(|l| l.contains("ReadFile"))
            .map(str::trim_end)
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        stream_of(&by_key),
        stream_of(&by_command),
        "a key and a command must fold the same way"
    );
    assert!(
        by_command.contains("/tools") || by_command.contains("ReadFile"),
        "the command ran:\n{by_command}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// ---- modals ---------------------------------------------------------------

// ---- runtime layout -------------------------------------------------------

// ---- more than one agent in the tree --------------------------------------

/// The screen is one conversation's fold, not the tree's.
///
/// The listener that feeds the modules sits at the root realm, and a realm
/// hears its descendants: every delegated member commits to its own log and
/// every one of those facts reaches that listener. Folding them all into one
/// stream interleaves two conversations by turn coordinate — a member's
/// thinking, its tool calls and its turn ends land in the lead's transcript.
/// What the lead may see of a member is what the member *told* it.
#[tokio::test]
async fn a_member_s_own_conversation_stays_off_the_lead_s_screen() {
    let dir = scratch("team-crosstalk");
    let script = replay(
        r#"{ text = "Delegating.", calls = [ { name = "team", args = { action = "delegate", name = "scout", role = "explorer", task = "look around" } } ] },
           { text = "Delegated." },
           { text = "Noted." }"#,
    );
    // The member is an `explorer` — a simple role — so it runs on the utility
    // slot, and its words are unmistakably its own.
    let member = format!(
        "[[insert]]\nname = \"team-in-process\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\n\
         config = {{ script = [ \
           {{ text = \"MEMBER-THINKING-OUT-LOUD\", calls = [ {{ name = \"tell_parent\", args = {{ text = \"scout reporting in\" }} }} ] }}, \
           {{ text = \"MEMBER-TRAILING-WORDS\" }} ] }}\n",
        dir = dir.to_string_lossy(),
    );
    let s = start(tree(&dir, &script, &[&member])).await;
    let task = s.open().await;

    s.term.type_line("have someone look around");
    s.quiet().await;

    let screen = s.screen();
    assert!(
        !screen.contains("MEMBER-THINKING-OUT-LOUD") && !screen.contains("MEMBER-TRAILING-WORDS"),
        "the member's own conversation is not the lead's screen:\n{screen}"
    );
    assert!(
        screen.contains("scout reporting in"),
        "what the member told the lead is on it:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The other half of the same rule: what the screen *may* show of a member.
///
/// The panel is the lead's own knowledge, drawn where it can be glanced at —
/// who it delegated to, with which role, whether they are still running, and
/// the last thing each of them said to it. Everything on it is either a fact
/// in this log or the agent registry's answer about now.
#[tokio::test]
async fn the_team_panel_says_who_is_on_the_team_and_what_each_last_said() {
    let dir = scratch("team-panel");
    let script = replay(
        r#"{ text = "Delegating.", calls = [ { name = "team", args = { action = "delegate", name = "scout", role = "explorer", task = "look around" } } ] },
           { text = "Delegated." },
           { text = "Noted." }"#,
    );
    let team = format!(
        "[[insert]]\nname = \"team-in-process\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\n\
         config = {{ script = [ \
           {{ text = \"MEMBER-THINKING-OUT-LOUD\", calls = [ {{ name = \"tell_parent\", args = {{ text = \"sessions are made in agent.rs\" }} }} ] }}, \
           {{ text = \"MEMBER-TRAILING-WORDS\" }} ] }}\n",
        dir = dir.to_string_lossy(),
    );
    let s = start(tree(&dir, &script, &[&team])).await;
    let task = s.open().await;

    // Before anyone is delegated to there is no panel at all: the row is
    // mounted, the panel asks for no rows, and the host places nothing — so the
    // conversation keeps the row rather than a blank line of chrome. A strip
    // saying "no members" would be the chrome this panel exists to avoid.
    assert!(
        s.term.last().unwrap().part("team").is_none(),
        "a session with no team has no team panel"
    );

    s.term.type_line("have someone look around");
    s.quiet().await;

    let panel = s
        .term
        .last()
        .unwrap()
        .part("team")
        .expect("the team panel is on screen")
        .lines
        .iter()
        .map(|l| l.plain())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(panel.contains("1 名成员"), "the count:\n{panel}");
    assert!(panel.contains("scout"), "the name:\n{panel}");
    assert!(panel.contains("explorer"), "the role:\n{panel}");
    assert!(
        panel.contains("sessions are made in agent.rs"),
        "the last thing it said to the lead:\n{panel}"
    );
    assert!(
        !panel.contains("MEMBER-THINKING-OUT-LOUD"),
        "and still nothing it said to itself:\n{panel}"
    );
    // Where it stands is not in this log: it comes over the connection, as the
    // member's own status. A member still on the team is never drawn as one
    // that has finished.
    assert!(
        !panel.contains("已结束"),
        "a live member is shown live:\n{panel}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// ---- the todo panel ------------------------------------------------------

/// The task list, as the model's own plan, on the screen.
///
/// The panel is a fold of the log rather than a list it keeps, so this also
/// checks the seam that matters: the plan the model sent and the plan the person
/// reads come from one derivation (`reduce_todos`), and an incremental update is
/// applied against the plan in force rather than accumulated on its own.
#[tokio::test]
async fn the_todo_panel_shows_the_plan_the_model_sent() {
    let dir = scratch("todo-panel");
    let script = replay(
        r#"{ text = "Planning.", calls = [
             { name = "todowrite", args = { todos = [
               { content = "读代码", status = "in_progress" },
               { content = "写面板", status = "pending" } ] } } ] },
           { text = "Started.", calls = [
             { name = "todowrite", args = { action = "update", id = 1, status = "completed" } } ] },
           { text = "Done." }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    // Before any plan there is no panel, not an empty one: the row is mounted,
    // the panel asks for no rows, and the host places nothing — so the
    // conversation keeps the row rather than a blank line of chrome.
    assert!(
        s.term.last().unwrap().part("todo").is_none(),
        "a session with no plan has no todo panel"
    );

    s.term.type_line("plan it");
    s.quiet().await;

    let panel = s
        .term
        .last()
        .unwrap()
        .part("todo")
        .expect("the todo panel is on screen")
        .lines
        .iter()
        .map(|l| l.plain())
        .collect::<Vec<_>>()
        .join("\n");
    // The plan, then one incremental patch to it: `#1` is completed by the
    // second call, so the header must read that plan's counts — the `update`
    // was applied against the list in force, not accumulated beside it.
    // 「待办」, not 「任务」: the header is the other front end's own word for this
    // panel now (`product::Msg::TodoPanelTitle`), so the two screens call it the
    // same thing — see `gates/tui-i18n.sh`'s second rule.
    assert!(panel.contains("待办"), "the header:\n{panel}");
    assert!(panel.contains("1 已完成"), "the patched count:\n{panel}");
    assert!(panel.contains("1 待办"), "{panel}");
    assert!(panel.contains("#1") && panel.contains("读代码"), "{panel}");
    assert!(panel.contains("#2") && panel.contains("写面板"), "{panel}");
    assert!(
        !panel.contains("todowrite"),
        "the call is not the plan:\n{panel}"
    );
    // Placed *and* painted: `part` says the host gave it a rect, and only the
    // flattened grid — the same one a screenshot and the exit dump come from —
    // says the row reached the screen.
    assert!(
        s.term.last().unwrap().rows().join("\n").contains("待办"),
        "the panel is on screen, not just in the parts list"
    );

    // Above the field, not under it. The panel used to place itself at the
    // bottom of the screen with `LayoutOp::Show`, which put it below the input
    // box — the last place anyone looks for what is being worked on.
    let screen = s.term.last().unwrap();
    let todo = screen.part("todo").expect("the task list").rect;
    let field = screen.part("input").expect("the field").rect;
    assert!(
        todo.bottom() <= field.y,
        "the task list sits above the field: todo {todo:?}, field {field:?}"
    );
    // And above the live line, when there is one: the composer's order is
    // task list, live line, tip, field.
    if let Some(live) = screen.part("live") {
        assert!(
            todo.bottom() <= live.rect.y,
            "the task list sits above the live line: todo {todo:?}, live {:?}",
            live.rect
        );
    } else {
        // No live line while idle, so the task list is simply above the tip
        // row's blank and the field's top rule.
        assert!(todo.y < field.y);
    }

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// And it goes away when the work does.
///
/// The panel is mounted for the whole session, so "it disappears" has to mean
/// the row went back to the conversation rather than that a blank strip was left
/// behind. That is the reason this is checked on the screen and not only in the
/// module's own tests: `Hug(0)` is what gives the row back, and a panel drawn
/// empty would pass a unit test while leaving a gap.
#[tokio::test]
async fn the_todo_panel_leaves_once_every_task_is_done() {
    let dir = scratch("todo-finished");
    let script = replay(
        r#"{ text = "Planning.", calls = [
             { name = "todowrite", args = { todos = [
               { content = "读代码", status = "in_progress" },
               { content = "写面板", status = "pending" } ] } } ] },
           { text = "One.", calls = [
             { name = "todowrite", args = { action = "update", id = 1, status = "completed" } } ] },
           { text = "Two.", calls = [
             { name = "todowrite", args = { action = "update", id = 2, status = "completed" } } ] },
           { text = "All done." }"#,
    );
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("plan it");
    s.quiet().await;

    let screen = s.term.last().unwrap();
    assert!(
        screen.part("todo").is_none(),
        "the plan is finished, so the panel is gone: {:?}",
        screen.part("todo").map(|p| p.lines.len())
    );
    // Gone, and not gone *blank*: the row it was using went back to the
    // conversation, so the last thing said is still on screen.
    let text = screen.rows().join("\n");
    assert!(text.contains("All done."), "{text}");

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// ---- approval ------------------------------------------------------------

/// An approval says who wants it.
///
/// A member's call is not the conversation's call: "allow `write_file`?" with
/// no name on it is a question the person cannot answer honestly, because the
/// thing they would be approving is not the thing they asked for.
#[tokio::test]
async fn an_approval_asked_for_by_a_member_says_which_member() {
    let dir = scratch("ask-member");
    let script = replay(
        r#"{ text = "Delegating.", calls = [ { name = "team", args = { action = "delegate", name = "scribe", role = "docs_writer", task = "write notes.md", scope = ["notes.md"] } } ] },
           { text = "Delegated." },
           { text = "Noted." }"#,
    );
    let asking = format!(
        "[[insert]]\nname = \"team-in-process\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[patch]]\nid = \"approval\"\ndisabled = true\n\n\
         [[patch]]\nid = \"approval-interactive\"\ndisabled = false\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\n\
         config = {{ script = [ \
           {{ text = \"writing\", calls = [ {{ name = \"write_file\", args = {{ file_path = \"notes.md\", content = \"hello\" }} }} ] }}, \
           {{ text = \"done\", calls = [ {{ name = \"tell_parent\", args = {{ text = \"wrote notes.md\" }} }} ] }} ] }}\n",
        dir = dir.to_string_lossy(),
    );
    let s = start(tree(&dir, &script, &[&asking])).await;
    let task = s.open().await;

    s.term.type_line("have someone write the notes");
    // Delegating is itself a risky call, so the lead's own `team` card comes
    // first. Allow it, and the member's card is the next one up.
    until(&s, "team").await;
    s.term.press(KeyPress::ch('1'));
    // Not `quiet`: the turn is deliberately stuck on a question, which is the
    // state under test. Settle on the card being up instead.
    until(&s, "write_file").await;
    let card = s.screen();
    assert!(card.contains("scribe"), "who is asking:\n{card}");
    assert!(card.contains("write_file"), "which tool:\n{card}");
    assert!(card.contains("notes.md"), "what it would do:\n{card}");
    assert!(
        card.contains("允许一次") && card.contains("总是允许") && card.contains("拒绝"),
        "and the three answers:\n{card}"
    );
    assert!(
        !card.contains("\"file_path\""),
        "the arguments are read, not dumped:\n{card}"
    );

    // Answer it: the member writes, and the file is there.
    s.term.press(KeyPress::ch('1'));
    s.quiet().await;
    assert!(
        dir.join("notes.md").exists(),
        "an allowed call runs:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Take the panel away and the screen still asks.
///
/// A question is drawn by a panel riding the stream's tail (`tui-panel-ask`), and
/// that row is a product's decision — not what makes a question answerable.
/// Without it the question is plain lines at the foot of the stream and the
/// keyboard answers them there, which is the path every front end that never
/// mounts the panel takes.
#[tokio::test]
async fn with_no_panel_row_the_question_is_still_asked_and_still_answered() {
    let dir = scratch("no-card");
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "plain" } } ] },
           { text = "Done." }"#,
    );
    let s = start(tree(
        &dir,
        &script,
        &[
            "[[patch]]\nid = \"approval\"\ndisabled = true\n",
            "[[patch]]\nid = \"approval-interactive\"\ndisabled = false\n",
            "[[remove]]\nid = \"tui-panel-ask\"\n",
        ],
    ))
    .await;
    let task = s.open().await;

    s.term.type_line("write it");
    until(&s, "write_file").await;
    let screen = s.screen();
    assert!(
        !screen.contains("↑↓ 选择"),
        "no row, no panel legend:\n{screen}"
    );
    assert!(
        screen.contains("允许一次"),
        "but the answers are there:\n{screen}"
    );
    // The composer is still there: with no panel to take its place, nothing may
    // have taken it away.
    assert!(
        screen.contains(caps_prompt()),
        "and so is the field, since nothing came to take its place:\n{screen}"
    );

    s.term.press(KeyPress::ch('1'));
    s.quiet().await;
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "plain",
        "and the key answered it:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The question panel rides the stream's tail, between the live line and the
/// steering bars, and it takes the composer's place while it is there.
///
/// Three claims at once, because they are one arrangement: where the panel is,
/// what is above it, and what is not on screen at all. Tested as geometry off the
/// frame rather than by looking for words, so "the composer hid" cannot pass by
/// the field merely being empty.
#[tokio::test]
async fn the_question_panel_rides_the_tail_and_takes_the_composer_s_place() {
    let dir = scratch("ask-panel-tail");
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "written" } } ] },
           { text = "Done." }"#,
    );
    let s = start(asking(&dir, &script)).await;
    let task = s.open().await;

    s.term.type_line("write it");
    until(&s, "↑↓ 选择").await;

    let frame = s.term.last().expect("a frame");
    let panel = frame
        .part("ask")
        .expect("the panel is placed under its own id")
        .rect;
    let stream = frame.part("stream").expect("the conversation").rect;
    let live = frame.part("live").map(|p| p.rect);
    let status = frame.part("status").expect("the status line").rect;

    // Under the conversation, and under the live line when there is one.
    assert!(
        panel.y >= stream.y + stream.h,
        "the panel is not below the conversation: panel {panel:?}, stream {stream:?}"
    );
    if let Some(live) = live {
        assert!(
            live.y + live.h <= panel.y,
            "the live line is not above the panel: live {live:?}, panel {panel:?}"
        );
    }
    // And above the status line, because the whole tail is.
    assert!(
        panel.y + panel.h <= status.y,
        "the panel ran into the status line: panel {panel:?}, status {status:?}"
    );

    // **The composer is gone.** Not empty — not placed: no field, no tip row.
    assert!(
        frame.part("input").is_none(),
        "the field is still on screen under the panel:\n{}",
        s.screen()
    );
    assert!(
        frame.part("tip").is_none(),
        "the tip row is still on screen:\n{}",
        s.screen()
    );
    // Nothing scrolled away either: the field's rows went to the conversation.
    assert!(
        stream.h > 1,
        "the conversation kept a row of its own: {stream:?}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The arrow keys pick, enter confirms, and the pointer does both.
///
/// The claim a highlighted row makes is that the row which is *lit* is the row a
/// confirm would take. Read off the drawn frame — the row whose background is the
/// brighter panel — because "the panel tracks the keys" and "the panel paints the
/// key's row" are two claims and only the second one is the feature.
#[tokio::test]
async fn the_question_panel_picks_by_key_and_by_pointer_and_confirms_the_lit_row() {
    let dir = scratch("ask-panel-pick");
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "written" } } ] },
           { text = "Done." }"#,
    );
    let s = start(asking(&dir, &script)).await;
    let task = s.open().await;

    s.term.type_line("write it");
    until(&s, "↑↓ 选择").await;

    let bright = Some(atomcode_tui::Color::role(
        atomcode_tui::theme::Role::PanelSelBg,
    ));
    // The screen row of the lit answer, read off the frame that is up.
    let lit_row = |s: &Session| -> Option<u16> {
        let part = s.term.last().expect("a frame").part("ask")?.clone();
        (0..part.lines.len())
            .find(|i| part.lines[*i].spans.iter().any(|sp| sp.style.bg == bright))
            .map(|i| part.rect.y + i as u16)
    };

    let panel = s.term.last().expect("a frame").part("ask").unwrap().rect;
    let first = lit_row(&s).expect("an answer starts lit");
    assert!(
        first >= panel.y && first < panel.y + panel.h,
        "the lit row is inside the panel: {first} vs {panel:?}"
    );

    // Down: the highlight moves to the next answer, and stays inside the panel.
    s.term.press(KeyPress::plain(Key::Down));
    // Not `quiet`: the turn is deliberately stuck on this question, so the screen
    // is never quiet until it is answered. Wait for the *move* instead — the one
    // thing that changed, which `until` can only see as the old row going dark.
    until_row(&s, first, false).await;
    let second = lit_row(&s).expect("an answer is lit after moving down");
    assert_ne!(
        second,
        first,
        "down did not move the highlight:\n{}",
        s.screen()
    );
    assert!(
        second > first,
        "down moved the highlight up: {first} -> {second}"
    );

    // Up: and back, so the arrows are a pair rather than one direction.
    s.term.press(KeyPress::plain(Key::Up));
    until_row(&s, first, true).await;
    assert_eq!(lit_row(&s), Some(first), "up did not come back");

    // The pointer takes over the same row. Hovering is enough: the row under the
    // pointer is the row a click would take, and the panel must say so.
    let last = panel.y + panel.h - 1;
    let target = (panel.y..=last)
        .rev()
        .find(|y| {
            let part = s.term.last().unwrap().part("ask").unwrap().clone();
            let i = (y - part.rect.y) as usize;
            part.lines
                .get(i)
                .is_some_and(|l| l.plain().contains("允许"))
        })
        .expect("an answer row to point at");
    s.term
        .pointer(atomcode_tui::surface::Click::Hover, panel.x + 2, target);
    until_row(&s, target, true).await;
    assert_eq!(
        lit_row(&s),
        Some(target),
        "the row the pointer is over is not the row drawn brighter:\n{}",
        s.screen()
    );

    // And the row that is lit is the row a click takes — not the first one, which
    // is what a click that ignored the pointer would have taken.
    s.term
        .pointer(atomcode_tui::surface::Click::Press, panel.x + 2, target);
    s.term
        .pointer(atomcode_tui::surface::Click::Release, panel.x + 2, target);
    s.quiet().await;
    assert!(
        !s.screen().contains("↑↓ 选择"),
        "the question is still up after a click on an answer:\n{}",
        s.screen()
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "written",
        "the answer the pointer picked is what was delivered:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Wait for the lit answer row to reach `row`, or for it to leave it.
///
/// `quiet` cannot be used while a question is up: the turn is deliberately stuck
/// on it, so the screen never settles. What is waited for instead is the one thing
/// that changes — which row carries the panel's highlight.
async fn until_row(s: &Session, row: u16, lit: bool) {
    let bright = Some(atomcode_tui::Color::role(
        atomcode_tui::theme::Role::PanelSelBg,
    ));
    let is_lit = |s: &Session| -> bool {
        let frame = match s.term.last() {
            Some(f) => f,
            None => return false,
        };
        let Some(part) = frame.part("ask") else {
            return false;
        };
        let Some(i) = row.checked_sub(part.rect.y).map(|i| i as usize) else {
            return false;
        };
        part.lines
            .get(i)
            .is_some_and(|l| l.spans.iter().any(|sp| sp.style.bg == bright))
    };
    for _ in 0..400 {
        if is_lit(s) == lit {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "row {row} never {} lit; last frame:\n{}",
        if lit { "became" } else { "stopped being" },
        s.screen()
    );
}

/// While a question is up the terminal must report where the pointer is.
///
/// The panel lights the row the pointer is over, and a hover is DECSET 1003 —
/// which the screen asks for only while something is following the pointer.
/// Handed in as a recorded event like the menu's own test, this passes whether or
/// not a real terminal would ever have sent one; asserted as the request, it does
/// not.
///
/// It is also the reason the heal arm had to learn about questions: a hover that
/// arrives while one is up is the answer to our own request, not a terminal that
/// took the mouse back.
#[tokio::test]
async fn a_question_asks_the_terminal_for_the_pointer_only_while_it_is_up() {
    let dir = scratch("ask-panel-motion");
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "written" } } ] },
           { text = "Done." }"#,
    );
    let s = start(asking(&dir, &script)).await;
    let task = s.open().await;

    s.quiet().await;
    assert!(
        !s.term.motion(),
        "the screen asks for free motion with nothing following the pointer"
    );

    s.term.type_line("write it");
    until(&s, "↑↓ 选择").await;
    assert!(
        s.term.motion(),
        "the panel follows the pointer, so the terminal has to be reporting it"
    );

    // Answer it: the panel goes, and so does the request — a terminal left in
    // 1003 sends an event for every cell the pointer crosses for the rest of the
    // session, for nothing.
    s.term.press(KeyPress::ch('2'));
    s.quiet().await;
    assert!(
        !s.screen().contains("↑↓ 选择"),
        "the question was answered:\n{}",
        s.screen()
    );
    assert!(
        !s.term.motion(),
        "the panel closed but the terminal is still reporting every cell"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The prompt marker this terminal draws.
fn caps_prompt() -> &'static str {
    atomcode_tui::caps::Caps::default().g(atomcode_tui::caps::Glyph::Prompt)
}

/// A model's answer arrives a delta at a time — a word here — and every delta
/// commits a fact and gets a second event out of the handle, so a word wakes
/// the loop twice. At a frame apiece the screen falls further behind the longer
/// the model talks, and the transcript on it is the thing being waited for.
/// What is already queued is answered in one frame instead, and this counts
/// frames against words because the whole point is that they are not the same
/// number.
#[tokio::test]
async fn a_burst_of_deltas_costs_frames_not_one_per_delta() {
    let dir = scratch("coalesce");
    let words = 400;
    let script = replay(&format!(r#"{{ text = "{}" }}"#, "ok ".repeat(words)));
    let s = start(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    let before = s.term.frame_count();
    s.term.type_line("go");
    s.quiet().await;
    let painted = s.term.frame_count() - before;
    println!("coalesce: {words} deltas -> {painted} frames");
    assert!(
        painted * 4 < words,
        "{words} deltas cost {painted} frames, so what is queued is not being drained into one frame"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A side-call model that never answers, so a member is busy until stopped.
struct Stalling;

#[async_trait]
impl atomcode_kernel::provider::LlmProvider for Stalling {
    fn model_name(&self) -> &str {
        "stalling"
    }
    async fn chat_stream(
        &self,
        _messages: &[atomcode_kernel::message::Message],
        _tools: &[atomcode_kernel::tool::ToolDef],
        _options: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        Ok(Box::pin(futures::stream::pending()))
    }
}

/// The same stand-in on the conversation's own model slot: a request that is
/// accepted and then says nothing, which is what a stalled gateway looks like.
struct StallingModelRow;

#[async_trait]
impl Plugin for StallingModelRow {
    fn name(&self) -> &'static str {
        "test-stalling-llm"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmSvc>(Arc::new(Stalling))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct StallingUtilityRow;

#[async_trait]
impl Plugin for StallingUtilityRow {
    fn name(&self) -> &'static str {
        "test-stalling-utility"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm-utility"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmUtilitySvc>(Arc::new(Stalling))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// `/cancel-all` stops the turn of the session on screen and of every member
/// of its team (`docs/adr/0023` §9): the member's turn ends cancelled, and since
/// it was the lead's errand the lead hears so. That the members stay is the
/// harness's to show: a cancel is not a stop.
#[tokio::test]
async fn cancel_all_stops_every_members_turn_and_keeps_the_team() {
    let dir = scratch("cancel-all");
    let script = replay(
        r#"{ text = "Delegating.", calls = [ { name = "team", args = { action = "delegate", name = "scout", role = "explorer", task = "look around" } } ] },
           { text = "Delegated." },
           { text = "Heard." }"#,
    );
    let member = format!(
        "[[insert]]\nname = \"team-in-process\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"test-stalling-utility\"\n",
        dir = dir.to_string_lossy(),
    );
    let s = start(tree(&dir, &script, &[&member])).await;
    let task = s.open().await;

    s.term.type_line("have someone look around");
    until(&s, "Delegated.").await;
    s.quiet().await;

    s.term.type_line("/cancel-all");
    until(&s, "1 个成员").await;
    until(&s, "Cancelled").await;

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A model that has not answered yet is said to be being waited for.
///
/// The inter-token budget is five minutes: a gateway that opens a stream and
/// then goes quiet leaves the screen with nothing new to draw for that long.
/// What must not happen is the screen looking like nothing was asked — the row
/// that says 正在等待模型, with its clock, is the whole difference between
/// "slow" and "stuck".
#[tokio::test]
async fn a_model_that_has_not_answered_yet_says_it_is_being_waited_for() {
    let dir = scratch("waiting-line");
    let stalling = "[[patch]]\nid = \"llm\"\nname = \"test-stalling-llm\"\n";
    let s = start(tree(&dir, &replay(r#"{ text = "unused" }"#), &[stalling])).await;
    let task = s.open().await;

    s.term.type_line("怎么做微调 ？");
    until(&s, "怎么做微调").await;
    for _ in 0..80 {
        if part_text(&s, "live").contains("正在等待模型") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let live = part_text(&s, "live");
    assert!(
        live.contains("正在等待模型"),
        "a turn waiting on the model must say so: {live:?}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The waiting row rises even when the turn's start arrives after its first fact.
///
/// The facts come by the feed and the turn's start by the runtime: two roads,
/// no order between them. With the fact first, the arm that followed waited for
/// a fact that had already gone by — so the row never rose, and against a model
/// that had opened its stream and gone quiet (five minutes of inter-token
/// budget) the screen said nothing at all for the whole turn. Reported
/// 2026-09-23: "5 分钟里屏幕底下没有「正在等待模型」那一行在转".
#[tokio::test]
async fn the_waiting_row_rises_even_when_the_turns_start_arrives_late() {
    let dir = scratch("late-turn-started");
    let stalling = "[[patch]]\nid = \"llm\"\nname = \"test-stalling-llm\"\n";
    // The start held back a second, so the fact is certain to win the race.
    let s = start_with_connection(
        tree(&dir, &replay(r#"{ text = "unused" }"#), &[stalling]),
        |connection| {
            let atomcode_host_api::HostConnection {
                session,
                commands,
                mut events,
                control,
            } = connection;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    if matches!(
                        event,
                        atomcode_kernel::event::AgentEvent::TurnStarted { .. }
                    ) {
                        let late = tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            let _ = late.send(event);
                        });
                        continue;
                    }
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            });
            atomcode_host_api::HostConnection {
                session,
                commands,
                events: rx,
                control,
            }
        },
    )
    .await;
    let task = s.open().await;

    s.term.type_line("怎么做微调 ？");
    until(&s, "怎么做微调").await;
    // Well inside the second the start is held for: what raises the row here is
    // the fact plus what the agent says it is doing, not the start.
    for _ in 0..20 {
        if part_text(&s, "live").contains("正在等待模型") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let live = part_text(&s, "live");
    assert!(
        live.contains("正在等待模型"),
        "the turn's own message is drawn and the agent says it is working, so the row is owed: {live:?}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// An error is said without taking the line off work that is still running.
///
/// Not every error ends a turn: a `/cancel-all` refused by an idle member, a
/// mid-turn cost warning, a persistence warning. Writing "idle" over any of
/// them took the spinner off a screen with a turn on it — and with it went the
/// answer to "is a turn running", which is what esc and the steering panel are
/// read against.
#[tokio::test]
async fn an_error_mid_turn_does_not_take_the_working_line_away() {
    let dir = scratch("error-mid-turn");
    let script = replay(
        r#"{ text = "Working.", calls = [ { name = "bash", args = { command = "sleep 2" } } ] },
           { text = "Done." }"#,
    );
    let (s, agent_said) = start_with_agent_events(tree(&dir, &script, &[])).await;
    let task = s.open().await;

    s.term.type_line("go");
    until(&s, "Working.").await;
    agent_said
        .send(atomcode_kernel::event::AgentEvent::Error {
            message: "本回合已经花了不少".into(),
            http_status: None,
            code: None,
            retryable: None,
        })
        .unwrap();
    until(&s, "本回合已经花了不少").await;
    // The turn's own row, not the transcript: a tool call's line says 运行中
    // whatever the screen believes about the turn.
    let live = part_text(&s, "live");
    assert!(
        live.contains("正在等待模型")
            || live.contains("正在思考")
            || live.contains("正在回复")
            || live.contains("正在运行"),
        "the turn is still running, and its own row must still say so: {live:?}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A stop the agent has finished does not go on saying 正在停止.
///
/// The words are a claim about now, and only a turn event takes them back — so
/// a turn whose end never reaches the screen (a runtime that answered the stop
/// and then said nothing more, which is the shape of the report this came from)
/// left them standing until the next turn. The agent saying it is idle, with
/// nothing sent to it unclaimed, is the backstop.
#[tokio::test]
async fn a_stop_the_agent_finished_does_not_keep_saying_it_is_stopping() {
    let dir = scratch("stopping-stuck");
    let script = replay(
        r#"{ text = "Working.", calls = [ { name = "bash", args = { command = "sleep 2" } } ] },
           { text = "Done." }"#,
    );
    // A connection that swallows the stop and loses the turn's ending — the two
    // halves of the report this came from. Everything else (the facts, the
    // status the agent announces) arrives as it does in life.
    let s = start_with_connection(tree(&dir, &script, &[]), |connection| {
        let atomcode_host_api::HostConnection {
            session,
            commands,
            mut events,
            control,
        } = connection;
        let (sent, mut typed) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(command) = typed.recv().await {
                if matches!(command, atomcode_kernel::event::AgentCommand::Cancel) {
                    continue;
                }
                if commands.send(command).is_err() {
                    break;
                }
            }
        });
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if matches!(
                    event,
                    atomcode_kernel::event::AgentEvent::TurnComplete { .. }
                        | atomcode_kernel::event::AgentEvent::Cancelled
                ) {
                    continue;
                }
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        atomcode_host_api::HostConnection {
            session,
            commands: sent,
            events: rx,
            control,
        }
    })
    .await;
    let task = s.open().await;

    s.term.type_line("go");
    until(&s, "Working.").await;
    s.term.press(KeyPress::plain(Key::Esc));
    until(&s, "正在停止").await;

    // The agent finishes the turn it was never told to stop, and says it is
    // idle. Nothing else will say so: the ending was dropped.
    for _ in 0..200 {
        if s.client().settled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(s.client().settled(), "the agent never went idle");

    // What is typed now opens a fresh turn, so it is not steering — and a screen
    // still claiming 停止中 reads it as exactly that, and shows it in the panel
    // as work the model is about to be handed.
    s.term.type_line("again");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let steering = part_text(&s, "steering");
    assert!(
        !steering.contains("again"),
        "a stale stopping claim made a fresh turn look like steering: {steering:?}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Back on a lead that is still working, esc stops it.
///
/// Looking at a member empties the screen's activity, and coming back only
/// restored it for members the roster knew to be working — never for the lead.
/// While a member is on screen the lead's turn events are not drawn, so nothing
/// else put it back either: the lead read as idle, and esc armed the idle
/// double-tap (a second press opened the rewind panel) instead of stopping it.
#[tokio::test]
async fn esc_stops_a_lead_that_is_still_working_after_looking_at_a_member() {
    let dir = scratch("esc-after-member");
    let script = replay(
        r#"{ text = "Delegating.", calls = [ { name = "team", args = { action = "delegate", name = "scout", role = "explorer", task = "look around" } } ] },
           { text = "Delegated." },
           { text = "Working.", calls = [ { name = "bash", args = { command = "sleep 5" } } ] },
           { text = "LEAD-FINISHED" }"#,
    );
    // A member that never answers: busy for the whole test, and never reporting
    // back into the lead's script.
    let member = format!(
        "[[insert]]\nname = \"team-in-process\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"test-stalling-utility\"\n",
        dir = dir.to_string_lossy(),
    );
    let s = start(tree(&dir, &script, &[&member])).await;
    let task = s.open().await;

    s.term.type_line("have someone look around");
    until(&s, "Delegated.").await;
    s.quiet().await;
    s.term.type_line("now take your time");
    until(&s, "Working.").await;

    s.term.press(KeyPress::plain(Key::Tab));
    until(&s, "Enter 切换").await;
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "正在看 scout").await;
    s.term.press(KeyPress::plain(Key::Tab));
    until(&s, "Enter 切换").await;
    s.term.press(KeyPress::plain(Key::Up));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "Delegated.").await;

    s.term.press(KeyPress::plain(Key::Esc));
    for _ in 0..80 {
        if s.screen().contains("已中断") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        s.screen().contains("已中断"),
        "esc on the lead did not stop its turn:\n{}",
        s.screen()
    );
    s.quiet().await;
    assert!(
        !s.screen().contains("LEAD-FINISHED"),
        "the lead ran on past the stop:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A lead that delegates to a member that never answers — busy until stopped —
/// and then works for a while itself.
fn team_with_a_busy_member(dir: &Path) -> (String, String) {
    let script = replay(
        r#"{ text = "Delegating.", calls = [ { name = "team", args = { action = "delegate", name = "scout", role = "explorer", task = "look around" } } ] },
           { text = "Delegated." },
           { text = "one", calls = [ { name = "bash", args = { command = "sleep 1" } } ] },
           { text = "two" }, { text = "three" }"#,
    );
    let member = format!(
        "[[insert]]\nname = \"team-in-process\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"test-stalling-utility\"\n",
        dir = dir.to_string_lossy(),
    );
    (script, member)
}

/// Put the member `scout` on screen through the team panel.
async fn look_at_the_member(s: &Session) {
    s.term.press(KeyPress::plain(Key::Tab));
    until(s, "Enter 切换").await;
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Enter));
    until(s, "正在看 scout").await;
}

/// Lines queued behind a member's turn, on its own screen, come back to the
/// composer when `esc` stops it — as the lead's do. The member's stop withdrew
/// them without a word to the screen, and a member's turn end never reaches
/// this connection, so nothing settled them either: stopped and gone.
#[tokio::test]
async fn esc_on_a_member_s_screen_puts_what_was_queued_back() {
    let dir = scratch("member-esc-queued");
    let (script, member) = team_with_a_busy_member(&dir);
    let s = start(tree(&dir, &script, &[&member])).await;
    let task = s.open().await;
    s.term.type_line("have someone look around");
    until(&s, "Delegated.").await;
    s.quiet().await;
    look_at_the_member(&s).await;

    s.term.type_line("MQ-u1");
    s.term.type_line("MQ-u2");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        s.screen().contains("MQ-u1") && s.screen().contains("MQ-u2"),
        "both are waiting behind the member's turn:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::plain(Key::Esc));
    for _ in 0..300 {
        if composer_text(&s).contains("MQ-u2") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let field = composer_text(&s);
    assert!(
        matches!(
            (field.find("MQ-u1"), field.find("MQ-u2")),
            (Some(a), Some(b)) if a < b
        ),
        "both are back in the composer, oldest first:\n{field}"
    );
    assert!(
        !user_messages(&s).iter().any(|m| m.contains("MQ-u")),
        "stopped means not sent: {:?}",
        user_messages(&s)
    );
    assert!(!s.screen().contains("没有送达"), "{}", s.screen());

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// `ctrl-x` on a member's screen sends what was queued to the member again.
#[tokio::test]
async fn ctrl_x_on_a_member_s_screen_sends_what_was_queued() {
    let dir = scratch("member-ctrl-x-queued");
    let (script, member) = team_with_a_busy_member(&dir);
    let s = start(tree(&dir, &script, &[&member])).await;
    let task = s.open().await;
    s.term.type_line("have someone look around");
    until(&s, "Delegated.").await;
    s.quiet().await;
    look_at_the_member(&s).await;

    s.term.type_line("MQ-w1");
    s.term.type_line("MQ-w2");
    tokio::time::sleep(Duration::from_millis(400)).await;

    s.term.press(KeyPress::ctrl('x'));
    for _ in 0..300 {
        if user_messages(&s).iter().any(|m| m == "MQ-w1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        user_messages(&s).iter().any(|m| m == "MQ-w1"),
        "the first line went to the member again: {:?}\n{}",
        user_messages(&s),
        s.screen()
    );
    // The member takes one message a step and never answers, so the second is
    // waiting in its inbox — listed, where a later stop can find it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        s.screen().contains("MQ-w2"),
        "the second is waiting behind it:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Leaving the lead's screen while its stop is still settling does not lose
/// what the stop took back: it is in the composer on the screen arrived at.
///
/// The stop settles at the lead's turn end, and a screen showing a member
/// never sees that; the view switch cleared the lines instead. Here the turn is
/// slow to end — it is holding a queued line in `PreStep` — so the switch lands
/// between the stop and the settling.
#[tokio::test]
async fn leaving_the_lead_mid_stop_keeps_what_the_stop_took_back() {
    let dir = scratch("switch-mid-stop");
    let (script, member) = team_with_a_busy_member(&dir);
    let s = start(tree(&dir, &script, &[&member, SLOW_PRE_STEP])).await;
    let task = s.open().await;
    s.term.type_line("have someone look around");
    until(&s, "Delegated.").await;
    s.quiet().await;

    s.term.type_line("now take your time");
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.term.type_line("SLOW-v1x");
    s.term.type_line("QUEUED-v2y");
    held_at_pre_step("SLOW-v1x").await;
    s.term.press(KeyPress::plain(Key::Esc));
    look_at_the_member(&s).await;

    for _ in 0..100 {
        if composer_text(&s).contains("QUEUED-v2y") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Past the held line's own refusal, which must not read as a failure.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let field = composer_text(&s);
    assert!(
        matches!(
            (field.find("SLOW-v1x"), field.find("QUEUED-v2y")),
            (Some(a), Some(b)) if a < b
        ),
        "what the stop took back is in the composer:\n{field}"
    );
    assert!(!s.screen().contains("没有送达"), "{}", s.screen());

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A team whose member speaks for itself, so its screen is recognisably its own.
fn team_with_a_talking_member(dir: &Path) -> (String, String) {
    let script = replay(
        r#"{ text = "Delegating.", calls = [ { name = "team", args = { action = "delegate", name = "scout", role = "explorer", task = "look around" } } ] },
           { text = "Delegated." },
           { text = "Noted." },
           { text = "Noted again." }"#,
    );
    let team = format!(
        "[[insert]]\nname = \"team-in-process\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\n\
         config = {{ script = [ \
           {{ text = \"MEMBER-THINKING-OUT-LOUD\", calls = [ {{ name = \"tell_parent\", args = {{ text = \"scout reporting in\" }} }} ] }}, \
           {{ text = \"MEMBER-TRAILING-WORDS\" }}, \
           {{ text = \"MEMBER-HEARD-YOU\" }} ] }}\n",
        dir = dir.to_string_lossy(),
    );
    (script, team)
}

/// One named region of the last frame, as plain text: the rows a module drew.
fn part_text(s: &Session, part: &str) -> String {
    s.term
        .last()
        .and_then(|frame| frame.part(part).cloned())
        .map(|part| {
            part.lines
                .iter()
                .map(|l| l.plain())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn panel_text(s: &Session) -> String {
    s.term
        .last()
        .and_then(|frame| frame.part("team").cloned())
        .map(|part| {
            part.lines
                .iter()
                .map(|l| l.plain())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The team panel is a way in (`docs/adr/0023` §3): Tab gives it the keyboard,
/// the arrows pick an agent, Enter puts it on screen — its own conversation,
/// and what is typed goes to it — and `主` brings the lead back.
#[tokio::test]
async fn the_keyboard_switches_the_screen_to_a_member_and_back() {
    let dir = scratch("switch-keys");
    let (script, team) = team_with_a_talking_member(&dir);
    let s = start(tree(&dir, &script, &[&team])).await;
    let task = s.open().await;

    s.term.type_line("have someone look around");
    until(&s, "scout reporting in").await;
    s.quiet().await;
    assert!(!s.screen().contains("MEMBER-THINKING-OUT-LOUD"));
    assert!(panel_text(&s).contains("主"), "{}", panel_text(&s));

    s.term.press(KeyPress::plain(Key::Tab));
    until(&s, "Enter 切换").await;
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "MEMBER-THINKING-OUT-LOUD").await;
    assert!(s.screen().contains("正在看 scout"), "{}", s.screen());
    let status = |s: &Session| {
        s.term
            .last()
            .and_then(|frame| frame.part("status").cloned())
            .map(|part| part.lines.iter().map(|l| l.plain()).collect::<String>())
            .unwrap_or_default()
    };
    assert!(
        status(&s).contains("成员 scout"),
        "the status line says whose screen: {}",
        status(&s)
    );
    assert!(
        !s.screen().contains("Delegated."),
        "the lead's conversation is off the screen:\n{}",
        s.screen()
    );

    s.term.type_line("one more thing");
    until(&s, "MEMBER-HEARD-YOU").await;

    s.term.press(KeyPress::plain(Key::Tab));
    until(&s, "Enter 切换").await;
    s.term.press(KeyPress::plain(Key::Up));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "Delegated.").await;
    assert!(
        !status(&s).contains("成员"),
        "the lead's again: {}",
        status(&s)
    );
    assert!(
        !s.screen().contains("MEMBER-THINKING-OUT-LOUD"),
        "the member's own conversation is off the lead's screen again:\n{}",
        s.screen()
    );
    assert!(
        s.screen().contains("one more thing"),
        "and the lead was told what the person said to it:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A press on a member's row switches to it, and the row under the pointer is
/// the row lit — the one a press would take.
#[tokio::test]
async fn a_press_on_a_team_row_switches_to_that_agent() {
    use atomcode_tui::surface::Click;

    let dir = scratch("switch-pointer");
    let (script, team) = team_with_a_talking_member(&dir);
    let s = start(tree(&dir, &script, &[&team])).await;
    let task = s.open().await;
    s.term.type_line("have someone look around");
    until(&s, "scout reporting in").await;
    s.quiet().await;

    let part = s.term.last().unwrap().part("team").unwrap().clone();
    let scout_row = part
        .lines
        .iter()
        .position(|l| l.plain().contains("scout"))
        .expect("a row for the member") as u16;
    let (x, y) = (part.rect.x + 2, part.rect.y + scout_row);

    s.term.pointer(Click::Hover, x, y);
    for _ in 0..100 {
        let lit = s
            .term
            .last()
            .and_then(|frame| frame.part("team").cloned())
            .is_some_and(|part| {
                part.lines
                    .get(scout_row as usize)
                    .is_some_and(|line| line.spans.iter().any(|span| span.style.bg.is_some()))
            });
        if lit {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        s.term
            .last()
            .and_then(|frame| frame.part("team").cloned())
            .is_some_and(|part| part.lines[scout_row as usize]
                .spans
                .iter()
                .any(|span| span.style.bg.is_some())),
        "the row under the pointer is lit"
    );
    s.term.pointer(Click::Press, x, y);
    s.term.pointer(Click::Release, x, y);
    until(&s, "MEMBER-THINKING-OUT-LOUD").await;
    assert!(s.screen().contains("正在看 scout"), "{}", s.screen());

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// Moving the mouse over the team panel does not take the keyboard
/// (`docs/adr/0023` §3).
///
/// The panel is up whenever there is a team, so a pointer that grabbed the
/// keyboard by crossing it would eat whatever the person was typing — every
/// keystroke routed to a panel that only eats `↑↓`, `Enter` and `Esc`. Only
/// `Tab`, pressed on purpose, hands it over.
#[tokio::test]
async fn hovering_the_team_panel_leaves_the_keyboard_where_it_was() {
    use atomcode_tui::surface::Click;

    let dir = scratch("team-hover-keeps-keys");
    let (script, team) = team_with_a_talking_member(&dir);
    let s = start(tree(&dir, &script, &[&team])).await;
    let task = s.open().await;
    s.term.type_line("have someone look around");
    until(&s, "scout reporting in").await;
    s.quiet().await;

    let part = s.term.last().unwrap().part("team").unwrap().clone();
    let scout_row = part
        .lines
        .iter()
        .position(|l| l.plain().contains("scout"))
        .expect("a row for the member") as u16;
    let (x, y) = (part.rect.x + 2, part.rect.y + scout_row);

    s.term.pointer(Click::Hover, x, y);
    // The row lights, which is what the pointer means — and the legend, which
    // is a list of keys, does not appear, because no keys have been handed over.
    for _ in 0..100 {
        let lit = s
            .term
            .last()
            .and_then(|frame| frame.part("team").cloned())
            .is_some_and(|part| {
                part.lines
                    .get(scout_row as usize)
                    .is_some_and(|line| line.spans.iter().any(|span| span.style.bg.is_some()))
            });
        if lit {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        s.term
            .last()
            .and_then(|frame| frame.part("team").cloned())
            .is_some_and(|part| part.lines[scout_row as usize]
                .spans
                .iter()
                .any(|span| span.style.bg.is_some())),
        "the row under the pointer is lit"
    );
    assert!(
        !s.screen().contains("Enter 切换"),
        "the panel has no keyboard, so it names no keys:\n{}",
        s.screen()
    );

    // And the keyboard is still the composer's: what is typed goes into the
    // field and reaches the agent on screen, which is still the lead.
    s.term.type_line("still typing here");
    until(&s, "still typing here").await;
    assert!(
        !s.screen().contains("正在看 scout"),
        "the pointer did not switch the screen either:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A command a row of the agent's tree puts in its catalog, for a person to run.
struct EchoCommand;

#[async_trait]
impl atomcode_harness::commands::CatalogCommand for EchoCommand {
    fn describe(&self) -> atomcode_kernel::agent::CommandDescription {
        atomcode_kernel::agent::CommandDescription {
            name: "echo".into(),
            usage: Some("<text>".into()),
            summary: "say it back".into(),
            target: atomcode_kernel::agent::CommandTarget::Session,
        }
    }
    async fn run(
        &self,
        _agent: Arc<atomcode_harness::agent::Agent>,
        args: &str,
    ) -> Result<String, String> {
        Ok(format!("echoed: {args}"))
    }
}

struct EchoCommandRow;

#[async_trait]
impl Plugin for EchoCommandRow {
    fn name(&self) -> &'static str {
        "test-echo-command"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["commands"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        atomcode_harness::commands::register(ctx, Arc::new(EchoCommand))
    }
}

/// A command a row of the agent's tree registers is on the screen's slash menu
/// and runs from it, with nothing about it written into the screen
/// (`docs/adr/0021` §10): listed as the agent describes it, run by name, its
/// output shown in the conversation.
#[tokio::test]
async fn a_command_the_agent_offers_is_on_the_slash_menu_and_runs() {
    let dir = scratch("catalog-cmd");
    let echo = "[[insert]]\nname = \"test-echo-command\"\n";
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[echo])).await;
    let task = s.open().await;

    // The menu narrows to it as it is typed: what it does, and — at the tail,
    // after the gloss, where an argument list moves no column — what it takes.
    s.term.type_text("/ec");
    until(&s, "say it back  <text>").await;
    assert!(s.screen().contains("/echo"), "{}", s.screen());
    for _ in 0..3 {
        s.term.press(KeyPress::plain(Key::Backspace));
    }

    s.term.type_line("/echo hello there");
    until(&s, "echoed: hello there").await;

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// What each model request asked for, recorded after the chain ran so every
/// row that writes the level has had its say.
static EFFORTS: std::sync::Mutex<Vec<Option<atomcode_kernel::provider::ReasoningEffort>>> =
    std::sync::Mutex::new(Vec::new());

struct EffortSpy;

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for EffortSpy {
    async fn handle(
        &self,
        req: &mut atomcode_harness::events::ModelRequest,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
    ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
    {
        let answered = next.run(req).await;
        EFFORTS.lock().unwrap().push(req.options.reasoning_effort);
        answered
    }
}

struct EffortSpyRow;

#[async_trait]
impl Plugin for EffortSpyRow {
    fn name(&self) -> &'static str {
        "test-effort-spy"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let _ =
            ctx.on_waterfall::<atomcode_harness::events::AgentRequest>(Arc::new(EffortSpy), true);
        Ok(())
    }
}

/// `/effort` must actually change what the agent's requests ask for, through
/// host control — not merely be listed in the menu, and not by reaching into
/// the agent's tree.
#[tokio::test]
async fn the_effort_command_changes_what_requests_ask_for_while_the_screen_runs() {
    let dir = scratch("effort-cmd");
    let spy = "[[insert]]\nname = \"test-effort-spy\"\n";
    let s = start(tree(
        &dir,
        &replay_effort(r#"{ text = "ok" }, { text = "ok" }"#, ALL_EFFORT_LEVELS),
        &[spy],
    ))
    .await;
    let task = s.open().await;

    s.term.type_line("before");
    s.quiet().await;
    assert_eq!(
        EFFORTS.lock().unwrap().last().copied().flatten(),
        None,
        "the base bundle mounts the row with no opinion"
    );

    s.term.type_line("/effort high");
    s.quiet().await;
    assert!(
        s.screen().contains("思考强度 → high"),
        "the command says so on screen:\n{}",
        s.screen()
    );

    // With no argument the levels appear inline in the slash menu — one row per
    // level, the way `/` lists the commands themselves — rather than a modal.
    // Typed without a return so the menu is still open to pick from; a pick
    // dispatches `/effort <level>`, the same path the typed form above took.
    s.term.type_text("/effort");
    for _ in 0..400 {
        if s.term.last().map(|f| f.part("menu").is_some()) == Some(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let menu = s
        .term
        .last()
        .unwrap()
        .part("menu")
        .expect("the levels open inline in the slash menu")
        .clone();
    let menu_text = menu
        .lines
        .iter()
        .map(|l| l.plain())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        menu_text.contains("effort high"),
        "a row per level:\n{menu_text}"
    );
    // The level already in force (`high`, set just above) is the one marked, so
    // the list says where the session stands before anything is picked.
    assert!(
        menu_text.contains('✓'),
        "the level in force is marked:\n{menu_text}"
    );

    // Down to the next level and take it. Which one that is depends on the
    // order of the table, so the level is read off the lit row rather than
    // assumed — the claim is that a pick reaches the same implementation a
    // typed argument does. The lit row is the one drawn on the selection
    // background, so the label is found by asking which known level it names.
    s.term.press(KeyPress::plain(Key::Down));
    s.quiet().await;
    let bright = Some(atomcode_tui::Color::role(
        atomcode_tui::theme::Role::PanelSelBg,
    ));
    let row = s
        .term
        .last()
        .unwrap()
        .part("menu")
        .unwrap()
        .lines
        .iter()
        .find(|l| l.spans.first().map(|sp| sp.style.bg) == Some(bright))
        .map(|l| l.plain())
        .expect("a lit row");
    let level = atomcode_harness::REASONING_EFFORT_LEVELS
        .iter()
        .find(|level| row.contains(**level))
        .copied()
        .unwrap_or("default")
        .to_string();
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, &format!("思考强度 → {level}")).await;

    // A value nothing parses is refused.
    s.term.type_line("/effort bogus");
    s.quiet().await;
    assert!(
        s.screen().contains("未知"),
        "an unknown level is refused:\n{}",
        s.screen()
    );

    s.term.type_line("after");
    s.quiet().await;
    assert_eq!(
        EFFORTS.lock().unwrap().last().copied().flatten(),
        atomcode_kernel::provider::ReasoningEffort::from_config(Some(level.as_str())),
        "the next request carries the level the panel picked"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_hover_with_nothing_following_the_pointer_says_the_mode_again_and_says_so() {
    // The terminal can put its own tracker back without telling us — a session
    // restore, a tab switch, a reset from anything else holding the tty. There
    // is no way to ask (see `Surface::heal_mouse`: the `$y` reply never drains
    // out of crossterm's parser), so the signal is behavioural: a plain move
    // arrives *only* while free motion is reporting, and free motion is asked
    // for exactly while the menu is up. A move with no menu is the one piece of
    // evidence available that the tracker is not where this side left it.
    let dir = scratch("heal-hover");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    let before = s.term.escapes().len();
    assert!(
        !s.screen().contains("鼠标被终端收回"),
        "nothing said before anything happened:\n{}",
        s.screen()
    );

    // A move, with no menu open. Nothing asked for it.
    let field = s
        .term
        .last()
        .expect("a frame")
        .part("input")
        .expect("the composer")
        .rect;
    s.term
        .pointer(atomcode_tui::surface::Click::Hover, field.x + 2, field.y);
    s.quiet().await;

    // Said again, as the whole mode — so a terminal that dropped the grab is
    // back in button reporting. The state does not change (this side never
    // thought it had changed), which is exactly why the tip is owed.
    let sent = s.term.escapes();
    assert!(sent.len() > before, "the mode was not said again: {sent:?}");
    assert_eq!(
        sent.last().map(String::as_str),
        Some(atomcode_tui::ansi::MOUSE_ON),
        "and it is the whole mode, not a delta"
    );
    assert_eq!(
        s.term.pointer_mode(),
        atomcode_tui::ansi::Pointer::Buttons,
        "still in button reporting — healing is not a state change"
    );

    // And the person is told, on the reserved row, because the click they were
    // about to make would have gone to the terminal instead.
    assert!(
        s.screen().contains("鼠标被终端收回"),
        "the person was not told:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_move_that_was_asked_for_says_nothing_again() {
    // The other half of the healing rule, and the half with a cost attached: an
    // arriving mouse event is itself proof that the tracker is on, so repeating
    // the mode on one buys nothing. While the menu is up it is worse than
    // nothing — free motion reports every cell the pointer crosses, so healing
    // per event would hand the terminal a packet per cell, which is the exact
    // price `MOUSE_MOTION_ON` is written to avoid paying.
    //
    // Counted as a burst rather than one event, so a tick landing in the window
    // cannot be read as the per-event healing this is about: ticks heal (that is
    // the other half of the rule), and at 110ms a short burst has room for very
    // few of them.
    let dir = scratch("heal-no-op");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    right_click_composer(&s);
    s.quiet().await;
    assert!(s.screen().contains("复制全文"), "the menu is up");

    let hovered = 24usize;
    let before = s.term.escapes().len();
    let field = s
        .term
        .last()
        .expect("a frame")
        .part("input")
        .expect("the composer")
        .rect;
    for i in 0..hovered {
        s.term.pointer(
            atomcode_tui::surface::Click::Hover,
            field.x + (i as u16 % 4),
            field.y,
        );
    }
    s.quiet().await;
    let grew = s.term.escapes().len() - before;

    assert!(
        grew * 2 < hovered,
        "{hovered} moves the menu asked for produced {grew} escapes — the mode \
         is being repeated per event, and the terminal is paying per cell"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_resumed_session_redraws_the_conversation_it_left_behind() {
    // What `atui --resume` is for. The log IS the snapshot (there is no other
    // format), so a resumed screen has to be rebuilt by folding the log the new
    // process just loaded — and the fold that runs while a session is live only
    // sees facts committed *after* it started. Measured before it was fixed:
    // resume came back to a blank screen with the whole conversation sitting in
    // the file, which is the one outcome a resume cannot have.
    let home = scratch("resume-home");
    let root = scratch("resume-work");
    let id = "fixed-id";

    {
        let s = start(tree_resumable(
            &root,
            &home,
            id,
            false,
            &replay(r#"{ text = "It is 42." }"#),
            &[],
        ))
        .await;
        let task = s.open().await;
        s.term.type_line("remember the number 42");
        s.quiet().await;
        // The writer is behind its own queue, so the turn being over is not the
        // same moment as the file having it.
        persisted(&home, id, 6).await;
        s.term.press(KeyPress::ctrl('d'));
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    let s = start(tree_resumable(
        &root,
        &home,
        id,
        true,
        &replay(r#"{ text = "still 42." }"#),
        &[],
    ))
    .await;
    let task = s.open().await;
    let screen = s.screen();
    assert!(
        screen.contains("remember the number 42"),
        "the resumed screen must show what was said before it:\n{screen}"
    );
    assert!(
        screen.contains("It is 42."),
        "…and what was answered:\n{screen}"
    );
    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// An approval survives a resume.
///
/// This was the one thing on screen that existed nowhere else. The question went
/// out through the `user-questions` seam and the answer came back as the tool's
/// result, so what a person decided was drawn on the transcript and written
/// down by nobody: a resumed session redrew the call that was approved with no
/// sign anyone had ever been asked, and a panel remounted mid-session lost the
/// answer the same way. `session.rs` states the rule this broke — what the
/// screen shows, the log records — so the fix is a fact, and this is the test
/// that says so from a **second process's** picture. Nothing here can pass by
/// the first screen still being on the wall.
#[tokio::test]
async fn a_resumed_session_shows_the_approval_it_was_given() {
    use atomcode_harness::session::SessionEvent;

    let home = scratch("approve-resume-home");
    let root = scratch("approve-resume-work");
    let id = "approved-id";
    let script = replay(
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "written" } } ] },
           { text = "Done." }"#,
    );
    // The resumable tree runs `yolo` by default: a resume test about approval
    // has to turn the asking on first, or there is no question to lose.
    let asking_here = [
        "[[patch]]\nid = \"approval\"\ndisabled = true\n",
        "[[patch]]\nid = \"approval-interactive\"\ndisabled = false\n",
    ];

    {
        let s = start(tree_resumable(
            &root,
            &home,
            id,
            false,
            &script,
            &asking_here,
        ))
        .await;
        let task = s.open().await;
        s.term.type_line("write it");
        // The turn is deliberately blocked on the answer, so this waits for the
        // question rather than for quiet.
        until(&s, "esc 拒绝").await;
        s.term.press(KeyPress::ch('1'));
        s.quiet().await;
        assert_eq!(
            std::fs::read_to_string(root.join("out.txt")).unwrap_or_default(),
            "written",
            "the allowed call ran"
        );
        // The writer is behind its own queue, so wait for the *fact* rather than
        // for the turn: a resume test that skipped this would be racing its own
        // fixture, and would sometimes pass on an empty file.
        let mut landed = false;
        for _ in 0..200 {
            if persisted_facts(&home, id)
                .iter()
                .any(|f| matches!(f, SessionEvent::Answered { .. }))
            {
                landed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(landed, "the answer was never written to the log");
        s.term.press(KeyPress::ctrl('d'));
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    let s = start(tree_resumable(
        &root,
        &home,
        id,
        true,
        &script,
        &asking_here,
    ))
    .await;
    let task = s.open().await;
    let screen = s.screen();
    assert!(
        screen.contains("→ 允许一次"),
        "the resumed screen must show the answer that was given:\n{screen}"
    );
    assert!(
        screen.contains("write_file") && screen.contains("out.txt"),
        "…about the call it was given for:\n{screen}"
    );
    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// ---- one stream per session (docs/adr/0022 §6) ----------------------------

/// A tree whose sessions persist under `home`, each App naming its own.
fn tree_persistent(root: &Path, home: &Path, script: &str) -> Setup {
    let sessions = home.join("sessions");
    let _ = std::fs::create_dir_all(&sessions);
    let persistence = format!(
        "[[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {sessions:?} }}\n"
    );
    setup(agent_base(root, &persistence, ""), script, &[])
}

/// Until the screen follows another session than `from`.
async fn moved_from(s: &Session, from: &str) -> String {
    for _ in 0..400 {
        let now = s.client().session();
        if now != from {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the screen never left {from}:\n{}", s.screen());
}

/// What the panel says when there is nothing else to go back to.
fn t_resume_no_others() -> String {
    atomcode_i18n::screen::t(atomcode_i18n::screen::Msg::ResumeNoOthers).into_owned()
}

/// 一次被拒的提交,话不会就这么没了。
///
/// 提交的那一刻输入框就清了,而被拒的提交**永远不会成为一条事实**
/// —— 于是它既不在屏上,也不在上箭头的历史里(历史是从
/// `SessionEvent::UserMessage` 折出来的)。打了多长都一样没了,而没登录、
/// 正在换 provider、刚按完 Esc 就回车 —— 这三种都会被拒。
///
/// 另一半是那句话自己:此前写的是 `{error:?}`,中文界面上一个英文枚举名。
#[tokio::test]
async fn a_refused_submit_hands_the_words_back_and_says_why_in_words() {
    let dir = scratch("refused");
    let (s, agent) = start_with_agent_events(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_line("这句话不应该消失 w3q");
    s.quiet().await;
    // 提交之后输入框是空的 —— 这正是后面要退回去的前提。
    assert!(
        !composer_text(&s).contains("w3q"),
        "提交清了输入框:{:?}",
        composer_text(&s)
    );

    agent
        .send(atomcode_kernel::event::AgentEvent::Rejected {
            command: "tui-1".into(),
            error: atomcode_kernel::event::CommandError::Unavailable,
        })
        .expect("the screen is listening");
    s.quiet().await;

    assert!(
        composer_text(&s).contains("w3q"),
        "话退回了输入框:{:?}\n{}",
        composer_text(&s),
        s.screen()
    );
    let screen = s.screen();
    assert!(
        !screen.contains("Unavailable"),
        "而不是一个枚举的名字:\n{screen}"
    );
    task.abort();
}

const SHELL_ROW_LAYER: &str = "[[insert]]\nname = \"tui-shell\"\n";

/// 把假 shell 挂上去的那一行 —— 和启动器那一行同形(`atomcode::tui_shell`)。
struct ShellPanelRow(Arc<dyn atomcode_tui::shell::Shell>);

#[async_trait]
impl Plugin for ShellPanelRow {
    fn name(&self) -> &'static str {
        "tui-shell"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-shell"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_tui::plugin::ShellSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// 开一块接了本机 shell 的屏幕,并把它发出去的命令都拄下来。
async fn start_with_shell(
    setup: Setup,
    shell: Arc<dyn atomcode_tui::shell::Shell>,
) -> (Session, Arc<std::sync::Mutex<Vec<String>>>) {
    let sent: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let slot = sent.clone();
    let session = start_full(
        setup,
        move |connection| {
            let atomcode_host_api::HostConnection {
                session,
                commands,
                events,
                control,
            } = connection;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Some(command) = rx.recv().await {
                    slot.lock()
                        .expect("sent poisoned")
                        .push(format!("{command:?}"));
                    if commands.send(command).is_err() {
                        break;
                    }
                }
            });
            atomcode_host_api::HostConnection {
                session,
                commands: tx,
                events,
                control,
            }
        },
        Ports {
            shell: Some(shell),
            ..Default::default()
        },
    )
    .await;
    (session, sent)
}

/// 一个假的本机 shell:不开进程,但记得被叫去跑什么。
#[derive(Default)]
struct Ran(std::sync::Mutex<Vec<String>>);

#[async_trait]
impl atomcode_tui::shell::Shell for Ran {
    async fn run(&self, command: &str, _within: Duration) -> atomcode_tui::shell::Ran {
        self.0
            .lock()
            .expect("ran poisoned")
            .push(command.to_string());
        atomcode_tui::shell::Ran {
            code: Some(0),
            output: format!("OUT-OF[{command}]"),
            timed_out: false,
        }
    }
}

/// `!cmd` 在本机跑,结果留在屏上,并跟**下一条**消息一起给模型。
///
/// 三半都要钉,而第三半是最容易漏的:
/// - `!` 不是发给模型的话 —— 不接这一手的话,`!git status` 会被当成
///   提示词发出去,而这正是上一代前端有、这一代没有的那件事;
/// - 结果要**留在对话区**,不是一条几秒就没的提示条 —— 一条命令的输出
///   可能是几十行,而人要回头看;
/// - 输出要**跟着下一条消息给模型**。没这一半的话,接下来那句「按上面
///   那个报错改一下」指的是模型没见过的东西 —— 而屏幕上一切看起来都正常。
#[tokio::test]
async fn a_bang_runs_here_and_what_it_printed_goes_with_the_next_message() {
    let dir = scratch("bang");
    let shell = Arc::new(Ran::default());
    let (s, sent) = start_with_shell(
        tree(&dir, &replay(r#"{ text = "ok" }"#), &[]),
        shell.clone(),
    )
    .await;
    let task = s.open().await;

    s.term.type_line("!git status");
    for _ in 0..200 {
        if !shell.0.lock().expect("ran poisoned").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        *shell.0.lock().expect("ran poisoned"),
        vec!["git status".to_string()],
        "跑的是 `!` 后面那一整行,而不是连 `!` 一起"
    );
    // 脚本只有一条回答。`!` 要是被当成提示词发出去了,它就会被花掉,
    // 屏上会出现 `ok` —— 这是「没有因此开一个回合」最直接的证据。
    assert!(
        !transcript(&s).contains("ok"),
        "这不是在对模型说话,不该开一个回合:\n{}",
        s.screen()
    );

    let mut seen = String::new();
    for _ in 0..200 {
        seen = transcript(&s);
        if seen.contains("OUT-OF[git status]") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        seen.contains("OUT-OF[git status]"),
        "输出留在对话区里:\n{}",
        s.screen()
    );

    // 下一条消息带着它走。钉的是**发出去的那条命令**,不是屏上画了
    // 什么 —— 这一半的整个意义就在于模型收到了什么。
    s.term.type_line("按上面那个改");
    for _ in 0..200 {
        if sent
            .lock()
            .expect("sent poisoned")
            .iter()
            .any(|c| c.contains("bash-output"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let wire = sent.lock().expect("sent poisoned").join("\n");
    assert!(
        wire.contains("bash-output") && wire.contains("OUT-OF[git status]"),
        "输出跟着下一条消息给了模型:\n{wire}"
    );
    assert!(
        wire.contains("按上面那个改"),
        "而话本身也在同一条里:\n{wire}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

const REMOTE_ROW_LAYER: &str = "[[insert]]\nname = \"tui-remote\"\n";

/// 把假远端挂上去的那一行 —— 和启动器那一行同形(`atomcode::tui_share`)。
struct RemotePanelRow(Arc<dyn atomcode_tui::remote::Remote>);

#[async_trait]
impl Plugin for RemotePanelRow {
    fn name(&self) -> &'static str {
        "tui-remote"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-remote"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_tui::plugin::RemoteSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// 一个假的另一端:问什么由测试推进去,答什么留在手里。
struct FarEnd {
    asks: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<String>>,
    said: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl atomcode_tui::remote::Remote for FarEnd {
    async fn next(&self) -> Option<String> {
        self.asks.lock().await.recv().await
    }
    fn said(&self, text: String) {
        self.said.lock().expect("said poisoned").push(text);
    }
}

/// 另一端请这块屏幕跑的命令,跑在这儿,答案回到那儿 —— 而会把这一侧地面
/// 换掉的那几条,拒。
///
/// 两半都要钉,而**拒的那一半才是理由**:
/// - 放行的那一半没有的话,手机上的 `/status` 与 `/goal` 按下去什么都不会
///   发生,而那一端只会看到一个不回话的按钮(上一代前端有这条路,这一代
///   直到现在没人接);
/// - 而要是把它做成「远端等同于本人」,手机上一个误触就能 `/cd` 掉这一侧
///   的工作目录 —— 那一端看不见这块屏幕上正开着什么。
#[tokio::test]
async fn the_far_end_runs_what_it_may_here_and_hears_back_and_is_refused_the_rest() {
    let dir = scratch("remote");
    let (ask, asks) = tokio::sync::mpsc::unbounded_channel();
    let far = Arc::new(FarEnd {
        asks: tokio::sync::Mutex::new(asks),
        said: std::sync::Mutex::new(Vec::new()),
    });
    let s = start_full(
        tree(&dir, &replay(r#"{ text = "ok" }"#), &[]),
        |connection| connection,
        Ports {
            remote: Some(far.clone()),
            ..Default::default()
        },
    )
    .await;
    let task = s.open().await;

    // 放行的那一半。
    ask.send("/status".into()).expect("the screen is listening");
    let mut heard = Vec::new();
    for _ in 0..200 {
        heard = far.said.lock().expect("said poisoned").clone();
        if !heard.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        heard.iter().any(|said| said.starts_with("/status\n")),
        "跑完的话回到了问的那一端:{heard:?}"
    );
    // 而键盘这一侧也知道那一端干了什么 —— 否则答案是凭空冒出来的。
    let seen = transcript(&s);
    assert!(
        seen.contains("/status"),
        "另一端做过什么,这块屏幕上也写着:\n{}",
        s.screen()
    );

    // 拒的那一半。
    ask.send("/cd /tmp".into())
        .expect("the screen is listening");
    for _ in 0..200 {
        if far.said.lock().expect("said poisoned").len() > heard.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let heard = far.said.lock().expect("said poisoned").clone();
    assert!(
        heard.iter().any(|said| said.starts_with("/cd /tmp\n")),
        "拒也要回话,否则那一端只是没反应:{heard:?}"
    );
    assert!(
        !transcript(&s).contains("/cd /tmp"),
        "而这一侧什么也没发生:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// 对话区里现在写着什么 —— 不含输入框与各种面板。
fn transcript(s: &Session) -> String {
    s.term
        .last()
        .expect("a frame")
        .part("stream")
        .map(|part| {
            part.lines
                .iter()
                .map(|l| l.plain())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// 输入框里现在写着什么。
fn composer_text(s: &Session) -> String {
    s.term
        .last()
        .expect("a frame")
        .part("input")
        .map(|part| {
            part.lines
                .iter()
                .map(|l| l.plain())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// A recording resume port: it says yes, and keeps what it was asked to throw
/// away.
#[derive(Default)]
struct DeletedSessions(std::sync::Mutex<Vec<String>>);

#[async_trait]
impl atomcode_tui::resume::Resume for DeletedSessions {
    async fn delete(&self, id: &str) -> Result<(), String> {
        self.0
            .lock()
            .expect("deleted poisoned")
            .push(id.to_string());
        Ok(())
    }
    async fn preview(&self, _id: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

/// Deleting a session from the panel takes two presses, and the second one
/// reaches the store.
///
/// **The wiring, which the panel's own criterion cannot see.** `resume::key`
/// answers the second `Delete` with `Step::Delete { id }`, and that has been
/// pinned as a pure function since the panel was written — but whether anything
/// carries that step to the port, and whether the row then leaves the list, is
/// three files away (`plugin.rs` dispatch → `ResumeSvc` → `forget_resume`). A
/// build with the dispatch arm missing keeps the unit criterion green and
/// quietly does nothing at all.
///
/// The first press is asserted too, and it is the half that costs something to
/// get wrong: one press that deleted would make a key people reach for on a
/// list into a key that throws work away.
#[tokio::test]
async fn a_session_is_deleted_from_the_panel_by_two_presses_and_not_by_one() {
    let home = scratch("resume-delete-home");
    let root = scratch("resume-delete-work");
    let store = Arc::new(DeletedSessions::default());
    let s = start_with_resume(
        tree_persistent(&root, &home, &replay(r#"{ text = "ANSWERED" }"#)),
        store.clone(),
    )
    .await;
    let task = s.open().await;

    // One session with something in it, then a second to be looking at —
    // `/resume` leaves out the one on screen, so the list needs another.
    s.term.type_line("a question");
    s.quiet().await;
    let first = s.client().session();
    persisted(&home, &first, 4).await;
    s.term.type_line("/clear");
    let second = moved_from(&s, &first).await;
    s.quiet().await;
    assert_ne!(first, second);

    s.term.type_line("/resume");
    s.quiet().await;
    // The row for the session left behind: its first prompt is its name, and
    // `N 轮 · …` is the metadata only a panel row carries — the conversation
    // above says the same words without them.
    assert!(
        s.screen().contains("1 轮 ·"),
        "the panel lists the session left behind:\n{}",
        s.screen()
    );

    // One press arms it and says so; it does not delete.
    s.term.press(atomcode_tui::surface::KeyPress::plain(
        atomcode_tui::surface::Key::Delete,
    ));
    s.quiet().await;
    assert!(
        store.0.lock().expect("deleted poisoned").is_empty(),
        "one press is not a delete"
    );

    // The second one goes through to the store, and the row leaves the list.
    s.term.press(atomcode_tui::surface::KeyPress::plain(
        atomcode_tui::surface::Key::Delete,
    ));
    for _ in 0..200 {
        if !store.0.lock().expect("deleted poisoned").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        *store.0.lock().expect("deleted poisoned"),
        vec![first.clone()],
        "the second press reached the store, once"
    );
    s.quiet().await;
    let after = s.screen();
    assert!(
        !after.contains("1 轮 ·"),
        "and the row it deleted is gone from the list:\n{after}"
    );
    assert!(
        after.contains(&t_resume_no_others()),
        "which leaves the list saying it is empty, not showing a stale row:\n{after}"
    );

    s.term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// The host replaces the session — a new one, then the first one again — and
/// the screen draws each in a stream of its own: nothing of the session it left
/// is drawn over the one it moved to, nothing is drawn twice, and the session
/// it moved to keeps drawing as it goes.
#[tokio::test]
async fn the_screen_moves_between_sessions_without_repeating_or_freezing() {
    let home = scratch("switch-home");
    let root = scratch("switch-work");
    let s = start(tree_persistent(
        &root,
        &home,
        &replay(r#"{ text = "THE-ANSWER" }, { text = "unused" }"#),
    ))
    .await;
    let task = s.open().await;

    s.term.type_line("first question");
    s.quiet().await;
    let first = s.client().session();
    assert!(s.screen().contains("THE-ANSWER"), "{}", s.screen());
    persisted(&home, &first, 4).await;

    s.term.type_line("/clear");
    let second = moved_from(&s, &first).await;
    s.quiet().await;
    let fresh = s.screen();
    assert!(
        !fresh.contains("first question"),
        "the session left behind is not drawn over the new one:\n{fresh}"
    );
    assert!(
        fresh.contains("已切换到会话"),
        "the switch is said:\n{fresh}"
    );

    s.term.type_line("second question");
    s.quiet().await;
    let live = s.screen();
    assert!(
        live.contains("second question") && live.contains("THE-ANSWER"),
        "the new session draws as it goes:\n{live}"
    );
    persisted(&home, &second, 4).await;

    s.term.type_line(&format!("/resume {first}"));
    let back = moved_from(&s, &second).await;
    assert_eq!(back, first);
    s.quiet().await;
    let resumed = s.screen();
    // The conversation, not the whole screen: the composer's upper rule also
    // carries the session's name, and the name of a session resumed from its
    // first prompt *is* that prompt. What this is about is the history being
    // folded once rather than twice.
    let conversation = s
        .term
        .last()
        .expect("a frame")
        .part("stream")
        .expect("the conversation")
        .lines
        .iter()
        .map(|l| l.plain())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        conversation.matches("first question").count(),
        1,
        "its history, drawn once:\n{resumed}"
    );
    assert!(
        !resumed.contains("second question"),
        "and nothing of the session it came from:\n{resumed}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// A host that says a turn would not be taken, and names what to run about it.
struct NotReady {
    inner: Arc<dyn atomcode_host_api::HostControl>,
    why: String,
    fix: Option<String>,
}

#[async_trait]
impl atomcode_host_api::HostControl for NotReady {
    async fn call(
        &self,
        command: atomcode_host_api::HostCommand,
    ) -> Result<atomcode_host_api::HostReply, atomcode_host_api::HostError> {
        if matches!(command, atomcode_host_api::HostCommand::Readiness { .. }) {
            return Ok(atomcode_host_api::HostReply::Readiness {
                ready: false,
                why: Some(self.why.clone()),
                fix: self.fix.clone(),
            });
        }
        self.inner.call(command).await
    }

    fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<atomcode_host_api::HostEvent> {
        self.inner.subscribe()
    }
}

/// The screen finds out that a turn would not be taken **before** anyone types.
///
/// What this is for: the front end used to learn that there is no provider by
/// submitting a turn and getting an error back — which tells a person after
/// they have written one, and on a new machine leaves the UI looking broken
/// rather than unconfigured. The old driver protocol had pre-flight checks for
/// this (`is_stopped`, `provider_unavailable_reason`, `accepts`); the bridge to
/// this screen never carried them over.
///
/// The host's words reach the screen as they stand — this front end does not
/// have the set of causes and must not paraphrase one.
#[tokio::test]
async fn the_screen_says_before_anyone_types_that_a_turn_would_not_be_taken() {
    let dir = scratch("readiness");
    let setup = tree(&dir, &replay(r#"{ text = "ok" }"#), &[]);
    let s = start_with_host(setup, |inner| {
        Arc::new(NotReady {
            inner,
            why: "还没有配置任何 provider——先加一个才能开始".into(),
            // Nothing to run about it: a host that has no answer says so, and
            // nothing is dispatched.
            fix: None,
        })
    })
    .await;
    let task = s.open().await;
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("还没有配置任何 provider"),
        "the host's own words, before a key was pressed:\n{screen}"
    );
    task.abort();
}

/// And the command the host names for it actually runs.
///
/// Separate from the half above because `/help` fills the screen and would push
/// the notice off it — which is a real property of a 24-row terminal, not a
/// test artefact. Each half is pinned where it can be seen.
#[tokio::test]
async fn the_command_a_host_names_for_an_unready_session_is_the_one_that_runs() {
    let dir = scratch("readiness-fix");
    let setup = tree(&dir, &replay(r#"{ text = "ok" }"#), &[]);
    let s = start_with_host(setup, |inner| {
        Arc::new(NotReady {
            inner,
            why: "登录已经失效".into(),
            // A command this build has and whose output nothing else produces,
            // so seeing it is proof the host's name was dispatched.
            fix: Some("help".into()),
        })
    })
    .await;
    let task = s.open().await;
    s.quiet().await;

    let screen = s.screen();
    // The end of the list rather than the start: `/help` is longer than 24
    // rows, so what is on screen is its tail.
    assert!(
        screen.contains("/whoami"),
        "the command the host named ran — this is `/help`'s output:\n{screen}"
    );
    task.abort();
}

/// And when that command opens a modal, the modal opens.
///
/// The bug this pins: a command reached by *picking* went down a different path
/// from a command reached by *typing*, and that path handled what a command
/// said and dropped everything else — so a pick whose command opened a modal
/// did nothing at all, silently. Readiness dispatches its `fix` the way a pick
/// is dispatched, which is how it was found; a wizard's last step is the same
/// shape.
///
/// `/view` and not `/model`: the first draft named `/model`, this fixture's
/// host refuses it, and the test passed on the word "模型" being in the
/// refusal. A criterion that green with the code under test removed is not a
/// criterion — so the command here is one the screen answers by itself.
#[tokio::test]
async fn a_command_a_host_names_that_opens_a_modal_opens_it() {
    let dir = scratch("readiness-modal");
    let note = dir.join("note.txt");
    std::fs::write(&note, "MODAL-CONTENT-ONLY-A-READER-SHOWS\n").expect("the file to look at");
    let setup = tree(&dir, &replay(r#"{ text = "ok" }"#), &[]);
    let shown = format!("view {}", note.display());
    let s = start_with_host(setup, move |inner| {
        Arc::new(NotReady {
            inner,
            why: "看看这个".into(),
            // Opens a reader rather than saying something, and the screen
            // answers it without the host — so what is on screen is the modal.
            fix: Some(shown),
        })
    })
    .await;
    let task = s.open().await;
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("MODAL-CONTENT-ONLY-A-READER-SHOWS"),
        "the modal the host's command opens is on screen:\n{screen}"
    );
    task.abort();
}

/// Work outside the loop has something to ring.
///
/// Everything that wakes this screen today is a key or the connection. A login
/// being polled behind a modal is neither: it changes what is on screen from a
/// thread the loop knows nothing about, and without this seam its change sits
/// there unpainted until the next keystroke — which, on the step that is
/// *waiting* for it, may never come.
///
/// Provided when the row mounts, not when the loop starts: an undeclared or
/// late-filled slot is invisible to the capability map, and the launcher's
/// audit says so. What it rings is filled in when the loop comes up — so
/// asking before that is a no-op rather than a panic, which is the honest
/// answer (there is no frame yet, and the first one is owed anyway).
#[tokio::test]
async fn something_working_outside_the_loop_can_ask_for_a_frame() {
    let dir = scratch("repaint");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let repaint = s
        .app
        .service::<atomcode_tui::plugin::RepaintSvc>()
        .expect("the row provides it when it mounts");
    // Nothing to ring yet, and that is not an error.
    repaint.now();

    let task = s.open().await;
    repaint.now();
    assert!(
        s.term
            .settle(Duration::from_millis(40), Duration::from_secs(5))
            .await,
        "and the screen is still painting after being rung"
    );
    task.abort();
}

/// A compaction that failed says so.
///
/// It used to fall through to "nothing on screen": `Compacted` had an arm and
/// `CompactionFailed` did not, so a `/compact` whose checkpoint could not be
/// written looked exactly like a command that never answered. Nothing else
/// reports it either — there is no notice kind for it, and the runtime's own
/// event is this one.
#[tokio::test]
async fn a_compaction_that_could_not_be_written_says_so() {
    let dir = scratch("compact-failed");
    let (s, agent) = start_with_agent_events(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    agent
        .send(atomcode_kernel::event::AgentEvent::CompactionFailed {
            trigger: atomcode_kernel::message::CompactTrigger::Manual { focus: None },
            error: atomcode_kernel::checkpoint::CompactionCheckpointError::new(
                "checkpoint 写不进去",
            ),
        })
        .expect("the screen is listening");
    for _ in 0..100 {
        if s.screen().contains("checkpoint 写不进去") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let screen = s.screen();
    assert!(
        screen.contains("没压缩成") && screen.contains("checkpoint 写不进去"),
        "the person is told, and told why:\n{screen}"
    );
    task.abort();
}

/// A resumed session remembers what was typed into it.
///
/// Written to settle a claim rather than to add a feature: the remaining-gaps
/// page had this down as "input history is in memory only, so it is lost on
/// restart", from a survey that read the push site and not where it is pushed
/// **from**. What an up-arrow walks is folded out of the log
/// (`Host::fold`, on `SessionEvent::UserMessage`), and a resume replays the
/// log — so the question is not what the code looks like, it is what comes
/// back. This answers it.
#[tokio::test]
async fn a_resumed_session_remembers_what_was_typed_into_it() {
    let home = scratch("history-home");
    let root = scratch("history-work");
    let id = "typed-into-twice";

    {
        let s = start(tree_resumable(
            &root,
            &home,
            id,
            false,
            &replay(r#"{ text = "ok" }"#),
            &[],
        ))
        .await;
        let task = s.open().await;
        s.term.type_line("port the quantizer to the NPU");
        s.quiet().await;
        persisted(&home, id, 6).await;
        s.term.press(KeyPress::ctrl('d'));
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    let s = start(tree_resumable(
        &root,
        &home,
        id,
        true,
        &replay(r#"{ text = "ok" }"#),
        &[],
    ))
    .await;
    let task = s.open().await;
    s.quiet().await;

    // Counted, not searched for: the line is **already** on screen — the
    // resumed transcript shows it — so "is it there" passes whether or not an
    // up-arrow does anything. The first draft of this asserted exactly that and
    // stayed green with the fold under test deleted. What is being judged is
    // that pressing Up puts a *second* copy of it in the composer.
    let count = |screen: &str| screen.matches("port the quantizer to the NPU").count();
    let before = s.screen();
    // Already on screen at least once — the resumed transcript shows it — and
    // the count is taken rather than assumed: what else carries the text is
    // the rest of the screen's business and has changed before. What is judged
    // is the **difference** one press makes.
    let seen = count(&before);
    assert!(seen > 0, "the resumed transcript has it:\n{before}");

    s.term.press(KeyPress::plain(Key::Up));
    s.quiet().await;
    let after = s.screen();
    assert_eq!(
        count(&after),
        seen + 1,
        "the up-arrow put it in the composer as well:\n{after}"
    );
    task.abort();
}

/// The layer that puts the test's tools panel on screen.
const TOOLS_PANEL_LAYER: &str = "[[insert]]\nname = \"tui-panel-tools\"\n";

/// The tools panel's view and its port, mounted the way the launcher's row
/// mounts them.
struct ToolsPanelRow(Arc<dyn atomcode_tui::tools::Tools>);

#[async_trait]
impl Plugin for ToolsPanelRow {
    fn name(&self) -> &'static str {
        "tui-panel-tools"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-tools"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let mods = ctx
            .require::<atomcode_tui::plugin::ModulesSvc>()
            .map_err(|e| e.to_string())?;
        mods.add_view(Arc::new(atomcode_tui::module::Mounted::<
            atomcode_tui::modules::tools::Tools,
        >::new()))?;
        let _ = ctx
            .provide::<atomcode_tui::plugin::ToolCatalogSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

const REWIND_PANEL_LAYER: &str = "[[insert]]\nname = \"tui-panel-rewind\"\n";

/// The rewind panel's view and its port, mounted the way the launcher's row
/// mounts them.
struct RewindPanelRow(Arc<dyn atomcode_tui::rewind::Rewind>);

#[async_trait]
impl Plugin for RewindPanelRow {
    fn name(&self) -> &'static str {
        "tui-panel-rewind"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-rewind"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let mods = ctx
            .require::<atomcode_tui::plugin::ModulesSvc>()
            .map_err(|e| e.to_string())?;
        mods.add_view(Arc::new(atomcode_tui::module::Mounted::<
            atomcode_tui::modules::rewind::Rewind,
        >::new()))?;
        let _ = ctx
            .provide::<atomcode_tui::plugin::RewindSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Start the screen with a rewind port behind it.
async fn start_with_rewind(setup: Setup, rewind: Arc<dyn atomcode_tui::rewind::Rewind>) -> Session {
    start_full(
        setup,
        |control| control,
        Ports {
            rewind: Some(rewind),
            ..Default::default()
        },
    )
    .await
}

/// Start the screen with a tools port behind it — the only way to see the
/// panel: a screen with no port refuses to open it.
async fn start_with_tools(setup: Setup, tools: Arc<dyn atomcode_tui::tools::Tools>) -> Session {
    start_full(
        setup,
        |control| control,
        Ports {
            tools: Some(tools),
            ..Default::default()
        },
    )
    .await
}

const RESUME_PANEL_LAYER: &str = "[[insert]]\nname = \"tui-panel-resume\"\n";

/// The resume panel's view and its port, mounted the way the launcher's row
/// mounts them (`atomcode::tui_resume`).
struct ResumePanelRow(Arc<dyn atomcode_tui::resume::Resume>);

#[async_trait]
impl Plugin for ResumePanelRow {
    fn name(&self) -> &'static str {
        "tui-panel-resume"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-resume", "tui-resume-store"]
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let mods = ctx
            .require::<atomcode_tui::plugin::ModulesSvc>()
            .map_err(|e| e.to_string())?;
        mods.add_view(Arc::new(atomcode_tui::module::Mounted::<
            atomcode_tui::modules::resume::Resume,
        >::new()))?;
        let _ = ctx
            .provide::<atomcode_tui::plugin::ResumeSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Start the screen with a resume port behind it.
async fn start_with_resume(setup: Setup, resume: Arc<dyn atomcode_tui::resume::Resume>) -> Session {
    start_full(
        setup,
        |control| control,
        Ports {
            resume: Some(resume),
            ..Default::default()
        },
    )
    .await
}

// ---- the plugins panel ---------------------------------------------------

/// A plugins port that answers from memory and records what it was asked to do.
///
/// A recording rather than a real marketplace, because what these judge is the
/// screen: that `/plugin` opens the panel, that a row leads to the job it
/// promises, and that the job's answer reaches the conversation. Whether `git`
/// can clone is `atomcode-capabilities`' business and is tested there.
#[derive(Default)]
struct Recorded {
    did: std::sync::Mutex<Vec<String>>,
    /// Set while a job is meant to be in flight, so the panel can be caught
    /// saying so.
    hold: Option<Duration>,
}

impl Recorded {
    fn rows(&self) -> atomcode_tui::plugins::PluginsView {
        use atomcode_tui::plugins::{MarketRow, PluginRow, Scope};
        let installed = self
            .did
            .lock()
            .expect("recording poisoned")
            .iter()
            .any(|line| line == "install tidy@official user");
        atomcode_tui::plugins::PluginsView::new(
            vec![
                PluginRow {
                    name: "tidy".into(),
                    marketplace: "official".into(),
                    description: "把代码排整齐".into(),
                    installed: installed.then_some(Scope::User),
                },
                PluginRow {
                    name: "lens".into(),
                    marketplace: "official".into(),
                    description: "看一眼改了什么".into(),
                    installed: None,
                },
            ],
            vec![MarketRow {
                name: "official".into(),
                source: "https://example.com/official.git".into(),
                plugins: 2,
                installed: usize::from(installed),
                updated: "今天更新".into(),
                official: true,
            }],
        )
    }

    fn note(&self, line: String) {
        self.did.lock().expect("recording poisoned").push(line);
    }
}

#[async_trait]
impl atomcode_tui::plugins::Plugins for Recorded {
    fn rows(&self) -> atomcode_tui::plugins::PluginsView {
        Recorded::rows(self)
    }
    async fn install(
        &self,
        plugin: &str,
        market: &str,
        scope: atomcode_tui::plugins::Scope,
    ) -> Result<String, String> {
        if let Some(hold) = self.hold {
            tokio::time::sleep(hold).await;
        }
        self.note(format!("install {plugin}@{market} {}", scope_word(scope)));
        Ok(format!("装好了 {plugin}@{market}"))
    }
    async fn update(
        &self,
        plugin: &str,
        market: &str,
        _scope: atomcode_tui::plugins::Scope,
    ) -> Result<String, String> {
        self.note(format!("update {plugin}@{market}"));
        Ok(format!("更新好了 {plugin}@{market}"))
    }
    async fn uninstall(
        &self,
        plugin: &str,
        market: &str,
        _scope: atomcode_tui::plugins::Scope,
    ) -> Result<String, String> {
        self.note(format!("uninstall {plugin}@{market}"));
        Ok(format!("卸掉了 {plugin}@{market}"))
    }
    async fn add_market(&self, url: &str) -> Result<String, String> {
        self.note(format!("add {url}"));
        Ok(format!("加上了市场 {url}"))
    }
    async fn update_market(&self, name: &str) -> Result<String, String> {
        self.note(format!("update-market {name}"));
        Ok(format!("市场 {name} 更新了"))
    }
    async fn remove_market(&self, name: &str) -> Result<String, String> {
        self.note(format!("remove-market {name}"));
        Ok(format!("删掉了市场 {name}"))
    }
    fn cancel(&self, job: &str) {
        self.note(format!("cancel {job}"));
    }
}

fn scope_word(scope: atomcode_tui::plugins::Scope) -> &'static str {
    match scope {
        atomcode_tui::plugins::Scope::User => "user",
        atomcode_tui::plugins::Scope::Project => "project",
        atomcode_tui::plugins::Scope::Local => "local",
    }
}

/// `/plugin` puts the panel up, a row leads to the install, and what the port
/// answered lands in the conversation.
///
/// The whole gesture end to end, because that is the thing that was missing:
/// the classic front end had this panel and the new screen had no way to reach
/// a plugin at all.
#[tokio::test]
async fn the_plugins_panel_opens_and_installs_what_was_picked() {
    let dir = scratch("plugins-install");
    let port = Arc::new(Recorded::default());
    let s = start_with_plugins(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/plugin");
    until(&s, "把代码排整齐").await;
    let open = s.screen();
    assert!(open.contains("全部"), "the pages are on screen:\n{open}");
    assert!(open.contains("市场"), "including the marketplaces:\n{open}");

    // The cursor opens on the first row, which is `lens` — sorted by name. Walk
    // to `tidy` and pick it: the form asks where it should go.
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "装到哪儿").await;
    let asked = s.screen();
    assert!(asked.contains("这台机器"), "the three scopes:\n{asked}");
    assert!(asked.contains("这个项目"), "{asked}");
    assert!(asked.contains("只有自己"), "{asked}");

    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "装好了 tidy@official").await;
    assert_eq!(
        port.did.lock().expect("recording poisoned").as_slice(),
        ["install tidy@official user"],
        "one install, with the scope that was picked"
    );
    task.abort();
}

/// While the job is out there the panel says so, and Esc gives up on it.
///
/// Both halves matter: a panel that swallowed every key without saying why
/// looks broken, and a person who walks away from a ten-second clone has to be
/// able to — with the port told, because only the port can undo what lands.
#[tokio::test]
async fn a_slow_install_can_be_waited_on_or_given_up_on() {
    let dir = scratch("plugins-slow");
    let port = Arc::new(Recorded {
        hold: Some(Duration::from_secs(30)),
        ..Recorded::default()
    });
    let s = start_with_plugins(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/plugin");
    until(&s, "把代码排整齐").await;
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "装到哪儿").await;
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "正在装 tidy@official").await;

    // Every key but Esc is swallowed while it runs.
    s.term.press(KeyPress::plain(Key::Enter));
    s.term.press(KeyPress::plain(Key::Down));
    assert!(
        s.screen().contains("正在装 tidy@official"),
        "still the one job:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::plain(Key::Esc));
    // The sentence the *answer* carries, not the one the legend does: while the
    // job runs the legend already says `esc 不等了`, so waiting for that would
    // be waiting for a frame that was already on screen.
    until(&s, "它落地之后会自己收拾干净").await;
    assert!(
        port.did
            .lock()
            .expect("recording poisoned")
            .iter()
            .any(|line| line == "cancel tidy@official"),
        "the port is told, because only it can undo what lands: {:?}",
        port.did.lock().expect("recording poisoned")
    );
    task.abort();
}

/// The same jobs, typed out — and a bare name that two marketplaces carry is
/// never guessed at.
#[tokio::test]
async fn plugin_subcommands_do_the_same_jobs_from_the_line() {
    let dir = scratch("plugins-typed");
    let port = Arc::new(Recorded::default());
    let s = start_with_plugins(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/plugin marketplace list");
    until(&s, "在册的市场").await;

    s.term.type_line("/plugin install tidy --scope project");
    until(&s, "装好了 tidy@official").await;
    assert_eq!(
        port.did.lock().expect("recording poisoned").as_slice(),
        ["install tidy@official project"],
        "`--scope project` is read as the scope, not as part of the name"
    );

    s.term.type_line("/plugin list");
    until(&s, "tidy@official").await;

    s.term.type_line("/plugin install nothing-by-that-name");
    until(&s, "没有叫 nothing-by-that-name 的插件").await;
    task.abort();
}

/// A screen with no plugins port says so rather than opening an empty panel.
#[tokio::test]
async fn a_screen_with_no_plugins_port_says_so() {
    let dir = scratch("plugins-absent");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/plugin");
    until(&s, "没有插件面板").await;

    s.term.type_line("/plugin list");
    until(&s, "没有接插件").await;
    task.abort();
}

// ---- the tools panel ------------------------------------------------------

/// A tools port that answers from memory and records what it was asked to do.
///
/// A recording rather than a live catalog, because what these judge is the
/// screen: that `/tools` opens the panel, that ⏎ on a row leads to the switch
/// it promises, and that what comes back is what gets drawn. Whether the switch
/// reaches the model's schema is judged where that happens
/// (`atomcode-harness/tests/tool_policy.rs`).
struct FakeTools {
    tools: std::sync::Mutex<Vec<atomcode_tui::tools::ToolRow>>,
    did: Arc<std::sync::Mutex<Vec<String>>>,
    /// Held shut while a test wants to see the panel mid-flight.
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl FakeTools {
    fn new(tools: Vec<atomcode_tui::tools::ToolRow>) -> Arc<Self> {
        Arc::new(Self {
            tools: std::sync::Mutex::new(tools),
            did: Arc::new(std::sync::Mutex::new(Vec::new())),
            gate: None,
        })
    }

    fn gated(
        tools: Vec<atomcode_tui::tools::ToolRow>,
        gate: Arc<tokio::sync::Notify>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tools: std::sync::Mutex::new(tools),
            did: Arc::new(std::sync::Mutex::new(Vec::new())),
            gate: Some(gate),
        })
    }

    fn view(&self) -> atomcode_tui::tools::ToolsView {
        atomcode_tui::tools::ToolsView::new(self.tools.lock().expect("tools poisoned").clone())
    }
}

fn tool(
    name: &str,
    owner: &str,
    state: atomcode_tui::tools::State,
) -> atomcode_tui::tools::ToolRow {
    atomcode_tui::tools::ToolRow {
        name: name.into(),
        owner: owner.into(),
        state,
    }
}

#[async_trait]
impl atomcode_tui::tools::Tools for FakeTools {
    async fn list(&self) -> Result<atomcode_tui::tools::ToolsView, String> {
        Ok(self.view())
    }

    async fn switch(
        &self,
        pattern: &str,
        on: bool,
    ) -> Result<atomcode_tui::tools::ToolsView, String> {
        if let Some(gate) = self.gate.as_ref() {
            gate.notified().await;
        }
        self.did
            .lock()
            .expect("recording poisoned")
            .push(format!("{} {pattern}", if on { "on" } else { "off" }));
        use atomcode_tui::tools::State;
        let mut tools = self.tools.lock().expect("tools poisoned");
        for row in tools.iter_mut() {
            // The port is where the config's answer is final: a switch never
            // reaches what the tree was built without.
            if row.name == pattern && row.state != State::Excluded {
                row.state = if on { State::On } else { State::Off };
            }
        }
        drop(tools);
        Ok(self.view())
    }
}

/// `/tools` pulls the panel up with what the host answered on it.
#[tokio::test]
async fn the_tools_panel_lists_what_the_model_can_call() {
    let dir = scratch("tools-panel");
    use atomcode_tui::tools::State;
    let port = FakeTools::new(vec![
        tool("read_file", "tool-fs-world", State::On),
        tool("write_file", "tool-fs-world", State::Off),
        tool("mcp__github__create_issue", "mcp-host", State::On),
    ]);
    let s = start_with_tools(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/toolbox");
    until(&s, "read_file").await;
    let open = s.screen();
    assert!(
        open.contains("write_file") && open.contains("mcp__github__create_issue"),
        "every name is on screen, whatever state it is in:\n{open}"
    );
    assert!(
        open.contains("2 个能调") && open.contains("1 个关掉的"),
        "the header counts what is on and what the person turned off:\n{open}"
    );
    assert!(
        open.contains("本次会话关掉的"),
        "and the off one says why it is off:\n{open}"
    );
    task.abort();
}

/// ⏎ on a row throws that tool's switch, and the panel draws the answer.
#[tokio::test]
async fn enter_on_a_row_turns_that_tool_off_and_the_panel_shows_it() {
    let dir = scratch("tools-switch");
    use atomcode_tui::tools::State;
    let port = FakeTools::new(vec![
        tool("read_file", "tool-fs-world", State::On),
        tool("write_file", "tool-fs-world", State::On),
    ]);
    let s = start_with_tools(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/toolbox");
    until(&s, "2 个能调").await;
    // The cursor opens on the first row, which is `read_file` — sorted by name.
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "1 个关掉的").await;

    assert_eq!(
        port.did.lock().expect("recording poisoned").as_slice(),
        ["off write_file"],
        "one switch, for the row the cursor was on"
    );
    let after = s.screen();
    assert!(
        after.contains("本次会话关掉的"),
        "and the row now says it is off:\n{after}"
    );
    task.abort();
}

/// A tool the config excluded has no switch to throw, and the panel says so
/// instead of sending a command that would do nothing.
#[tokio::test]
async fn a_tool_the_config_excluded_is_not_switchable_from_the_panel() {
    let dir = scratch("tools-excluded");
    use atomcode_tui::tools::State;
    let port = FakeTools::new(vec![tool("write_file", "tool-fs-world", State::Excluded)]);
    let s = start_with_tools(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/toolbox");
    until(&s, "write_file").await;
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "改配置才能放回来").await;

    assert!(
        port.did.lock().expect("recording poisoned").is_empty(),
        "and nothing was sent: there is no switch for it"
    );
    task.abort();
}

/// Typing filters, so forty MCP tools are a list you can get through.
#[tokio::test]
async fn typing_in_the_tools_panel_narrows_it() {
    let dir = scratch("tools-filter");
    use atomcode_tui::tools::State;
    let port = FakeTools::new(vec![
        tool("read_file", "tool-fs-world", State::On),
        tool("mcp__github__create_issue", "mcp-host", State::On),
        tool("mcp__jira__create_issue", "mcp-host", State::On),
    ]);
    let s = start_with_tools(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/toolbox");
    until(&s, "read_file").await;
    for c in "github".chars() {
        s.term.press(KeyPress::plain(Key::Char(c)));
    }
    // Waited on the *disappearance*: every name was already on screen, so a
    // wait for one of them would pass before the first key was even read.
    until_gone(&s, "read_file").await;
    let narrowed = s.screen();
    assert!(
        !narrowed.contains("mcp__jira__create_issue") && !narrowed.contains("read_file"),
        "only what matches is listed:\n{narrowed}"
    );
    task.abort();
}

/// While a switch is out there, the panel says so and takes no key but Esc —
/// otherwise every press sends another switch on top of the first.
#[tokio::test]
async fn the_tools_panel_takes_no_key_but_esc_while_a_switch_is_in_flight() {
    let dir = scratch("tools-busy");
    use atomcode_tui::tools::State;
    let gate = Arc::new(tokio::sync::Notify::new());
    let port = FakeTools::gated(
        vec![
            tool("read_file", "tool-fs-world", State::On),
            tool("write_file", "tool-fs-world", State::On),
        ],
        gate.clone(),
    );
    let s = start_with_tools(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/toolbox");
    until(&s, "2 个能调").await;
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "正在关掉 read_file").await;

    s.term.press(KeyPress::plain(Key::Enter));
    s.term.press(KeyPress::plain(Key::Down));
    s.quiet().await;
    assert!(
        port.did.lock().expect("recording poisoned").is_empty(),
        "nothing else went out: the first switch has not landed yet"
    );

    gate.notify_waiters();
    until(&s, "1 个关掉的").await;
    assert_eq!(
        port.did.lock().expect("recording poisoned").as_slice(),
        ["off read_file"],
        "and exactly one switch was sent"
    );
    task.abort();
}

/// The typed form, for a pattern that would be a lot of ⏎ in a list.
#[tokio::test]
async fn tools_off_typed_out_says_what_moved() {
    let dir = scratch("tools-typed");
    use atomcode_tui::tools::State;
    let port = FakeTools::new(vec![tool("write_file", "tool-fs-world", State::On)]);
    let s = start_with_tools(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/toolbox off write_file");
    until(&s, "关掉了:write_file").await;
    assert_eq!(
        port.did.lock().expect("recording poisoned").as_slice(),
        ["off write_file"],
    );
    task.abort();
}

/// A screen whose launcher gave it no port says so, rather than opening a panel
/// with nothing in it.
#[tokio::test]
async fn a_screen_with_no_tools_port_says_so() {
    let dir = scratch("tools-absent");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/toolbox");
    until(&s, "tui-panel-tools").await;
    task.abort();
}

// ---- the rewind panel -----------------------------------------------------

/// A rewind port that answers from memory and records what it was asked to do.
///
/// A recording rather than a live session, because what these judge is the
/// screen: that a double-tap on Esc opens the panel, that a turn and a scope
/// leave together, and that what came back lands where the person types.
/// Whether a workspace can really be put back is judged where that happens
/// (`atomcode-capabilities`' checkpoint tests).
struct FakeRewind {
    view: atomcode_tui::rewind::RewindView,
    did: std::sync::Mutex<Vec<String>>,
    /// What the host hands back, as the words of the turn that was taken back.
    restored: Option<String>,
}

impl FakeRewind {
    fn new(view: atomcode_tui::rewind::RewindView, restored: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            view,
            did: std::sync::Mutex::new(Vec::new()),
            restored: restored.map(str::to_string),
        })
    }
}

#[async_trait]
impl atomcode_tui::rewind::Rewind for FakeRewind {
    async fn points(&self) -> Result<atomcode_tui::rewind::RewindView, String> {
        Ok(self.view.clone())
    }

    async fn rewind(
        &self,
        turn: u64,
        scope: atomcode_tui::rewind::Scope,
    ) -> Result<atomcode_tui::rewind::Done, String> {
        self.did
            .lock()
            .expect("recording poisoned")
            .push(format!("{turn} {scope:?}"));
        Ok(atomcode_tui::rewind::Done {
            prompt: self.restored.clone(),
            files: 3,
        })
    }
}

/// Two turns: one that changed a file and one that changed nothing.
fn two_turns() -> atomcode_tui::rewind::RewindView {
    use atomcode_tui::rewind::{Change, Point};
    atomcode_tui::rewind::RewindView::new(
        vec![
            Point {
                turn: 1,
                prompt: "写个解析器".into(),
                changes: vec![Change {
                    path: "parser.rs".into(),
                    additions: 484,
                    deletions: 12,
                }],
                code: true,
            },
            Point {
                turn: 2,
                prompt: "再加一个测试".into(),
                changes: Vec::new(),
                code: false,
            },
        ],
        None,
    )
}

/// **Esc twice pulls the rewind panel up.** The gesture a person reaches for
/// when the last turn went the wrong way: stop, then take it back.
#[tokio::test]
async fn a_double_tap_on_esc_pulls_the_rewind_panel_up() {
    let dir = scratch("rewind-double-esc");
    let port = FakeRewind::new(two_turns(), None);
    let s = start_with_rewind(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    // One press is not the gesture: it is the ordinary Escape, and it leaves the
    // screen alone.
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    assert!(
        !s.screen().contains("写个解析器"),
        "一下 Esc 不该拉起面板:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::plain(Key::Esc));
    until(&s, "写个解析器").await;
    let open = s.screen();
    assert!(
        open.contains("parser.rs") && open.contains("+484"),
        "每个回合底下压着它动过什么:\n{open}"
    );
    assert!(
        open.contains("没有代码改动"),
        "没动过文件的那个回合也要说出来:\n{open}"
    );
    task.abort();
}

/// The latch is the quiet kind: anything else between the two presses means the
/// second one is an ordinary Escape again, not half a gesture.
#[tokio::test]
async fn work_between_two_escapes_is_not_a_double_tap() {
    let dir = scratch("rewind-latch-drops");
    let port = FakeRewind::new(two_turns(), None);
    let s = start_with_rewind(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.press(KeyPress::plain(Key::Esc));
    s.term.press(KeyPress::plain(Key::Char('h')));
    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    assert!(
        !s.screen().contains("写个解析器"),
        "中间干过别的,第二下 Esc 就只是 Esc:\n{}",
        s.screen()
    );
    task.abort();
}

/// **A turn and a scope leave together, and the words come back to the
/// composer.** Two presses of ⏎: the first picks the turn, the second says what
/// goes back with it.
#[tokio::test]
async fn choosing_a_turn_and_a_scope_sends_that_rewind_and_returns_the_words() {
    let dir = scratch("rewind-go");
    let port = FakeRewind::new(two_turns(), Some("写个解析器"));
    let s = start_with_rewind(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/rewind");
    until(&s, "写个解析器").await;
    // The panel rests on "(current)"; two steps up is the turn that changed a
    // file, which is the only one a code rewind is offered for.
    s.term.press(KeyPress::plain(Key::Up));
    s.term.press(KeyPress::plain(Key::Up));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "把什么一起带回去").await;
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "已还原 3 个文件").await;

    assert_eq!(
        port.did.lock().expect("recording poisoned").as_slice(),
        ["1 Both"],
        "一次回退,带着人挑的那一档范围"
    );
    let after = s.screen();
    assert!(
        !after.contains("把什么一起带回去"),
        "落地之后面板收起来:\n{after}"
    );
    assert!(
        after.contains("写个解析器"),
        "而被撤回的那句话回到人打字的地方:\n{after}"
    );
    task.abort();
}

/// **The panel opens aimed at nothing.** It rests on "(current)", where ⏎ puts
/// it away without rewinding anything — a panel that opened aimed at a rewind
/// would be one stray press from taking work back.
#[tokio::test]
async fn the_rewind_panel_opens_aimed_at_nothing() {
    let dir = scratch("rewind-rests");
    let port = FakeRewind::new(two_turns(), None);
    let s = start_with_rewind(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/rewind");
    until(&s, "（当前）").await;
    s.term.press(KeyPress::plain(Key::Enter));
    until_gone(&s, "写个解析器").await;
    assert!(
        port.did.lock().expect("recording poisoned").is_empty(),
        "什么都没回退"
    );
    task.abort();
}

/// A turn that changed no file is not offered a code rewind: the scope is
/// refused with the reason, rather than quietly doing the conversation instead.
#[tokio::test]
async fn a_turn_with_no_file_refuses_the_code_scope_in_the_panel() {
    let dir = scratch("rewind-no-code");
    let port = FakeRewind::new(two_turns(), None);
    let s = start_with_rewind(tree(&dir, &replay(r#"{ text = "ok" }"#), &[]), port.clone()).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/rewind");
    until(&s, "再加一个测试").await;
    // One step up from "(current)" is the turn that changed nothing.
    s.term.press(KeyPress::plain(Key::Up));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "把什么一起带回去").await;
    s.term.press(KeyPress::plain(Key::Down));
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "没改过文件").await;

    assert!(
        port.did.lock().expect("recording poisoned").is_empty(),
        "没派出去:那一档按下去本来就什么都不会发生"
    );
    task.abort();
}

/// A screen whose launcher gave it no rewind panel says so, rather than opening
/// a panel with nothing in it.
#[tokio::test]
async fn a_screen_with_no_rewind_port_says_so() {
    let dir = scratch("rewind-absent");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    s.term.type_line("/rewind");
    until(&s, "tui-panel-rewind").await;
    task.abort();
}

// ---- the execution mode: a key, a command, and the badge they move -------

/// The status row as one string.
fn status_row(s: &Session) -> String {
    s.term
        .last()
        .and_then(|frame| frame.part("status").cloned())
        .map(|part| part.lines.iter().map(|l| l.plain()).collect::<String>())
        .unwrap_or_default()
}

/// Wait for the status row to say `text`.
///
/// Reading the row rather than the whole screen, because what is asserted is
/// that the *row* carries something — a string that also appears in a command's
/// own output would otherwise pass while the row said nothing.
async fn until_status(s: &Session, text: &str) {
    for _ in 0..400 {
        if status_row(s).contains(text) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "the status row never said `{text}`: {:?}\n{}",
        status_row(s),
        s.screen()
    );
}

/// The thinking level the agent describes itself with reaches the status row,
/// against the model — tuix's shape, `model [high]`.
///
/// The level is set on the session by `/effort`, and the row draws it from the
/// agent's *description* rather than from the command's own output: the command
/// saying `思考强度 → high` is about the turn to come, while the row is about
/// what the session is now. So the description is what is pushed here, which is
/// the road the row reads.
///
/// Injected rather than driven through `/effort`, because the tree these tests
/// mount is a bare one: its host patches the row on a level change and never
/// re-describes, so a level set that way is genuinely not described back. What
/// is worth judging here is the screen's half — that a described level lands on
/// the row beside the model — which is exactly what injection isolates.
#[tokio::test]
async fn the_described_thinking_level_lands_on_the_status_row_beside_the_model() {
    let dir = scratch("effort-row");
    let (s, agent) = start_with_agent_events(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.quiet().await;

    // Nothing is drawn before a level is described: "the endpoint's default
    // stands" is not a level, and inventing one would be a claim about the
    // session that nothing made.
    assert!(
        !status_row(&s).contains('['),
        "a level was drawn with nothing described: {:?}",
        status_row(&s)
    );

    let session = s.client().root();
    let described = s.client().described().unwrap_or_default();
    agent
        .send(atomcode_kernel::event::AgentEvent::Described {
            description: Box::new(atomcode_kernel::agent::AgentDescription {
                session,
                reasoning_effort: Some(atomcode_kernel::provider::ReasoningEffort::High),
                ..described
            }),
        })
        .expect("the screen is listening");

    until_status(&s, "[high]").await;

    // Against the model, not adrift at the end of the row: the level is how
    // *this* model is driven, so it is drawn immediately after its name.
    let row = status_row(&s);
    let level = row.find("[high]").expect("the level");
    let sep = row.find('│').expect("the separator after the model");
    assert!(level < sep, "the level is not against the model: {row:?}");
    task.abort();
}

/// A host that takes the mode switch and can say the mode changed back.
///
/// The cycle is one exchange with two halves — the key asks, the host answers —
/// so the fixture stands in for both: it records what was asked, keeps the mode
/// it was told to be in, and holds the channel the screen subscribed to. That is
/// the same shape `atomcode::host` gives a real host (minus the runtime behind
/// it): a `SetMode` moves the mode, a `Mode` reads it back, and a change is
/// announced. Anything else is passed through, so a screen that asks its host
/// something else on the way up still gets an answer.
struct ModeHost {
    inner: Arc<dyn atomcode_host_api::HostControl>,
    asked: Arc<std::sync::Mutex<Vec<atomcode_host_api::Mode>>>,
    /// What this host says the session's mode is. `None` until something sets
    /// one, which is the state a real host is in only before its startup flag —
    /// and it is what makes the screen's own pull meaningful.
    current: Arc<std::sync::Mutex<Option<atomcode_host_api::Mode>>>,
    /// The screen's own subscription, taken once when it starts.
    said: Arc<
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<atomcode_host_api::HostEvent>>>,
    >,
}

impl ModeHost {
    fn announce(&self, session: &str, mode: atomcode_host_api::Mode) {
        *self.current.lock().expect("current poisoned") = Some(mode);
        self.push(atomcode_host_api::HostEvent::ModeChanged {
            session: session.to_string(),
            mode,
        });
    }

    /// 这块 e2e 里唯一一个**能往屏幕推事件**的宿主,所以别的宿主事件也从这儿
    /// 推 —— 再写一个只为了推另一种事件的假宿主,是同一件事的第二份。
    fn push(&self, event: atomcode_host_api::HostEvent) {
        let said = self.said.lock().expect("said poisoned");
        said.as_ref()
            .expect("the screen subscribed before it could be told")
            .send(event)
            .expect("the screen is still there to hear it");
    }
}

#[async_trait]
impl atomcode_host_api::HostControl for ModeHost {
    async fn call(
        &self,
        command: atomcode_host_api::HostCommand,
    ) -> Result<atomcode_host_api::HostReply, atomcode_host_api::HostError> {
        use atomcode_host_api::{HostCommand, HostReply};
        match command {
            HostCommand::SetMode { mode, .. } => {
                self.asked.lock().expect("asked poisoned").push(mode);
                *self.current.lock().expect("current poisoned") = Some(mode);
                Ok(HostReply::Done)
            }
            // The reading half, answered from what this host was told — the same
            // way `atomcode::host` reads its runtime's flags, minus the runtime.
            HostCommand::Mode { .. } => Ok(HostReply::Mode {
                mode: *self.current.lock().expect("current poisoned"),
            }),
            other => self.inner.call(other).await,
        }
    }

    fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<atomcode_host_api::HostEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.said.lock().expect("said poisoned") = Some(tx);
        rx
    }
}

/// Start the screen with a mode-recording host behind it, and a settings port
/// when the test needs one.
///
/// `seed` is the mode this host holds *before* the screen connects, which is the
/// whole point of the reading half: a host whose startup flag already set one
/// must be able to say so, and `None` is the host that has nothing to report.
async fn start_with_mode_host(
    setup: Setup,
    settings: Option<Arc<dyn atomcode_tui::settings::Settings>>,
    seed: Option<atomcode_host_api::Mode>,
) -> (Session, Arc<ModeHost>) {
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let said = Arc::new(std::sync::Mutex::new(None));
    let current = Arc::new(std::sync::Mutex::new(seed));
    let made = Arc::new(std::sync::Mutex::new(None::<Arc<ModeHost>>));
    let s = start_full(
        setup,
        {
            let asked = asked.clone();
            let said = said.clone();
            let current = current.clone();
            let made = made.clone();
            move |connection| {
                let atomcode_host_api::HostConnection {
                    session,
                    commands,
                    events,
                    control,
                } = connection;
                let host = Arc::new(ModeHost {
                    inner: control,
                    asked: asked.clone(),
                    current: current.clone(),
                    said: said.clone(),
                });
                *made.lock().expect("made poisoned") = Some(host.clone());
                atomcode_host_api::HostConnection {
                    session,
                    commands,
                    events,
                    control: host as Arc<dyn atomcode_host_api::HostControl>,
                }
            }
        },
        Ports {
            settings,
            ..Default::default()
        },
    )
    .await;
    let host = made
        .lock()
        .expect("made poisoned")
        .clone()
        .expect("the wrapper ran");
    (s, host)
}

/// Shift+Tab steps the mode on, the host is told which one, and the status row
/// says what the host said back.
///
/// All three halves in one test, because the feature *is* the loop: a key that
/// computes the next mode but never tells the host would pass a key-only
/// assertion, and a badge drawn from the screen's own guess rather than the
/// host's answer would agree with itself while disagreeing with the session.
#[tokio::test]
async fn shift_tab_cycles_the_mode_and_the_status_row_says_which_one() {
    let dir = scratch("mode-cycle");
    let (s, host) = start_with_mode_host(
        tree(&dir, &replay(r#"{ text = "ok" }"#), &[]),
        None,
        // What a coding host always reports: a session starts in `ask`, and the
        // screen only learned that because it asked (`HostCommand::Mode`).
        Some(atomcode_host_api::Mode::Ask),
    )
    .await;
    let task = s.open().await;
    s.quiet().await;
    let session = s.client().root();

    // The default mode is not drawn: a badge on every screen would be chrome
    // that stopped meaning "somebody changed something".
    let idle = status_row(&s);
    assert!(
        !idle.contains("accept edits"),
        "the default is drawn: {idle:?}"
    );
    assert!(!idle.contains("auto"), "the default is drawn: {idle:?}");

    // One step: ask → accept edits.
    s.term.press(KeyPress::plain(Key::BackTab));
    for _ in 0..200 {
        if !host.asked.lock().expect("asked poisoned").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        host.asked.lock().expect("asked poisoned").as_slice(),
        &[atomcode_host_api::Mode::AcceptEdits],
        "shift+tab did not ask the host for the next mode"
    );
    // The host says so, and the row follows — it draws the host's answer, not
    // a guess of its own.
    host.announce(&session, atomcode_host_api::Mode::AcceptEdits);
    until(&s, "accept edits").await;

    // And on again: accept edits → auto, off the mode the host pushed.
    s.term.press(KeyPress::plain(Key::BackTab));
    for _ in 0..200 {
        if host.asked.lock().expect("asked poisoned").len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        host.asked.lock().expect("asked poisoned").last(),
        Some(&atomcode_host_api::Mode::Auto),
        "the second step did not come off the mode the host pushed"
    );
    task.abort();
}

/// Plain Tab does *not* cycle the mode by default: it is the completion menu's
/// and the team panel's.
///
/// The half that makes the setting worth having. Stealing Tab from the menu
/// would break completion on every terminal, and this is the assertion that
/// says the default still belongs to it.
#[tokio::test]
async fn plain_tab_leaves_the_mode_alone_unless_the_setting_says_otherwise() {
    let dir = scratch("mode-tab-default");
    let (s, host) = start_with_mode_host(
        tree(&dir, &replay(r#"{ text = "ok" }"#), &[]),
        None,
        Some(atomcode_host_api::Mode::Ask),
    )
    .await;
    let task = s.open().await;
    s.quiet().await;

    s.term.press(KeyPress::plain(Key::Tab));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        host.asked.lock().expect("asked poisoned").is_empty(),
        "plain tab cycled the mode under the default setting"
    );
    task.abort();
}

/// A settings port that answers `ui.mode_switch_key` and nothing else.
///
/// A `Choice`, because that is what the setting is — the criterion is about the
/// value the screen reads, and a fixture that lied about the kind would be
/// describing a different setting.
struct TabSwitch(&'static str);

impl atomcode_tui::settings::Settings for TabSwitch {
    fn rows(&self) -> atomcode_tui::settings::SettingsView {
        atomcode_tui::settings::SettingsView::new(vec![atomcode_tui::settings::SettingRow {
            id: atomcode_tui::settings::MODE_SWITCH_KEY.to_string(),
            label: "模式切换键".to_string(),
            value: self.0.to_string(),
            kind: atomcode_tui::settings::SettingKind::Choice(vec![
                "shift_tab".to_string(),
                "tab".to_string(),
            ]),
            applies: atomcode_tui::settings::Applies::Immediately,
        }])
    }
    fn set(&self, _id: &str, _value: &str) -> Result<atomcode_tui::settings::SettingsView, String> {
        Err("this port cannot write".into())
    }
    fn reset(&self, _id: &str) -> Result<atomcode_tui::settings::SettingsView, String> {
        Err("this port cannot write".into())
    }
}

/// A mode the host set **before this screen subscribed** still reaches the row.
///
/// The gap this closes, and the reason the contract grew a reading half: the
/// screen learns the mode from `HostEvent::ModeChanged`, and an event pushed to
/// nobody is an event nobody heard. A host that seeds its mode at startup — the
/// product does exactly this for `--dangerously-skip-permissions`, which sets
/// `Auto` before the front end has connected — used to leave the row claiming
/// the session was an ordinary one while it was in fact running without asking.
///
/// Nothing is pushed here on purpose. The screen must find this out by *asking*,
/// which is the behaviour under test; a test that announced the change would
/// pass whether or not the pull exists.
#[tokio::test]
async fn a_mode_set_before_the_screen_connected_still_reaches_the_row() {
    let dir = scratch("mode-seeded");
    let (s, _host) = start_with_mode_host(
        tree(&dir, &replay(r#"{ text = "ok" }"#), &[]),
        None,
        // Already `auto` when the screen arrives, as `--dangerously-skip-permissions`
        // leaves it. No `announce` follows: the event this seeds predates the
        // subscription and was dropped, which is the whole situation.
        Some(atomcode_host_api::Mode::Auto),
    )
    .await;
    let task = s.open().await;
    s.quiet().await;

    // The badge, from the host's answer rather than from anything pushed.
    let row = status_row(&s);
    assert!(
        row.contains("auto"),
        "a session running without asking drew as an ordinary one: {row:?}"
    );
    task.abort();
}

/// 宿主猜的「接下来也许可以说」,一路走到编辑区里。
///
/// 这一条钉的是**整条线**:事件到了屏幕、画在了field 下面、→ 把它收下、收下
/// 之后它成为一条真的用户消息。三段接线里少任何一段,屏幕上都只是「什么都没
/// 发生」—— 而运行时那一侧一直在为每个自己结束的回合采一次样,采完丢掉。
///
/// **还钉了「收下之后它就没了」**:留着的话,人把它删掉之后它会再冒出来,
/// 而那正是「删掉」要表达的意思的反面。
#[tokio::test]
async fn a_guess_at_what_to_say_next_reaches_the_field_and_right_takes_it() {
    let dir = scratch("suggested");
    let (s, host) = start_with_mode_host(
        tree(&dir, &replay(r#"{ text = "ok" }"#), &[]),
        None,
        Some(atomcode_host_api::Mode::Ask),
    )
    .await;
    let task = s.open().await;
    s.quiet().await;
    let session = s.client().root();

    host.push(atomcode_host_api::HostEvent::Suggested {
        session: session.clone(),
        text: "接着把登录那条补上".into(),
    });
    let mut shown = String::new();
    for _ in 0..200 {
        shown = composer_text(&s);
        if shown.contains("接着把登录那条补上") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        shown.contains("接着把登录那条补上"),
        "猜的那句话画在编辑区那一块里:\n{}",
        s.screen()
    );
    // 不再带键名:那句话本身就是建议,画在输入行里(与补全同一个位置)。

    s.term.press(KeyPress::plain(Key::Right));
    for _ in 0..200 {
        if composer_text(&s).contains("❯ 接着把登录那条补上") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let shown = composer_text(&s);
    assert!(
        shown.contains("❯ 接着把登录那条补上"),
        "→ 把它收进了输入行:\n{}",
        s.screen()
    );
    assert!(
        !shown.contains('→'),
        "收下之后那一行就该没了,否则它会再被收一次:\n{shown}"
    );

    // 而它现在是一句真的话:回车发出去,对话区里就有它。
    s.term.press(KeyPress::plain(Key::Enter));
    let mut seen = String::new();
    for _ in 0..200 {
        seen = transcript(&s);
        if seen.contains("接着把登录那条补上") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        seen.contains("接着把登录那条补上"),
        "收下的那句话要能像自己打的一样发出去:\n{}",
        s.screen()
    );
    task.abort();
}

/// Plain `Tab` takes the guess too — the key the reference front end's hint row
/// named (`Tab: …`), and the one this screen's row now names beside `→`.
///
/// Pinned with `ui.mode_switch_key = "tab"` on purpose: that is the one setting
/// where plain Tab is somebody else's key, and a guess on an empty line still
/// wins — so Tab always means what the row advertising it says. Without the
/// guess, the same setting cycles (the test below).
#[tokio::test]
async fn a_guess_at_what_to_say_next_is_taken_by_plain_tab() {
    let dir = scratch("suggested-tab");
    let (s, host) = start_with_mode_host(
        tree(&dir, &replay(r#"{ text = "ok" }"#), &[]),
        Some(Arc::new(TabSwitch("tab"))),
        Some(atomcode_host_api::Mode::Ask),
    )
    .await;
    let task = s.open().await;
    s.quiet().await;
    let session = s.client().root();

    host.push(atomcode_host_api::HostEvent::Suggested {
        session,
        text: "接着把登录那条补上".into(),
    });
    let mut shown = String::new();
    for _ in 0..200 {
        shown = composer_text(&s);
        if shown.contains("接着把登录那条补上") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // 建议直接画在输入行里,不再写怎么按下它 —— 但这两个键仍旧收得下它。
    assert!(
        composer_text(&s).contains("接着把登录那条补上"),
        "猜的那句话写在输入行里:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::plain(Key::Tab));
    for _ in 0..200 {
        if composer_text(&s).contains("❯ 接着把登录那条补上") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let shown = composer_text(&s);
    assert!(
        shown.contains("❯ 接着把登录那条补上"),
        "Tab 把它收进了输入行:\n{}",
        s.screen()
    );
    assert!(
        host.asked.lock().expect("asked poisoned").is_empty(),
        "收下那句话和切模式是两件事,那一按只能干前一件"
    );

    // And it takes it only on an empty line. With words in the field — here the
    // same sentence, now in the history, typed back as a prefix — plain Tab is
    // the mode key again, and it does **not** quietly complete from history
    // instead: that would be a key nobody advertised ahead of one this row does.
    //
    // Judged on the mode and not on the text: the completion is drawn as a dim
    // ghost *inside* the line (`modules::input`), so scraping the composer cannot
    // tell "offered" from "taken" — while a Tab that took something never
    // reaches the mode key at all.
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    s.term.type_text("接着");
    s.term.press(KeyPress::plain(Key::Tab));
    for _ in 0..200 {
        if !host.asked.lock().expect("asked poisoned").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        host.asked.lock().expect("asked poisoned").as_slice(),
        &[atomcode_host_api::Mode::AcceptEdits],
        "行里有字的时候,Tab 还是那个模式键"
    );
    task.abort();
}

/// With `ui.mode_switch_key = "tab"` plain Tab cycles — the setting, not a
/// second key.
#[tokio::test]
async fn the_tab_setting_moves_the_cycle_onto_plain_tab() {
    let dir = scratch("mode-tab-setting");
    let (s, host) = start_with_mode_host(
        tree(&dir, &replay(r#"{ text = "ok" }"#), &[]),
        Some(Arc::new(TabSwitch("tab"))),
        Some(atomcode_host_api::Mode::Ask),
    )
    .await;
    let task = s.open().await;
    s.quiet().await;

    s.term.press(KeyPress::plain(Key::Tab));
    for _ in 0..200 {
        if !host.asked.lock().expect("asked poisoned").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        host.asked.lock().expect("asked poisoned").as_slice(),
        &[atomcode_host_api::Mode::AcceptEdits],
        "the tab setting did not move the cycle onto plain tab"
    );
    task.abort();
}
