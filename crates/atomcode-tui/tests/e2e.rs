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

fn agent_catalog() -> PluginRegistry {
    let mut c = atomcode_harness::plugins::catalog();
    c.register(Arc::new(HoldTurnEnd));
    c.register(Arc::new(EffortSpyRow));
    c.register(Arc::new(EchoCommandRow));
    c.register(Arc::new(StallingUtilityRow));
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
    let connection = {
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
    };
    let screen = Screen {
        headless: Some((80, 24)),
        ..Screen::default()
    };
    let extra: Vec<&str> = setup.screen.iter().map(String::as_str).collect();
    let mounted = launch::mount(&screen, &extra, connection)
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
    assert!(opening.contains("快速上手"), "the tips heading:\n{opening}");
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

#[tokio::test]
async fn a_resumed_session_does_not_open_with_a_welcome() {
    // The judgement behind "the stream is empty is the whole test for a new
    // session", and the reason `open_conversation` runs **after** the history is
    // folded in: a resumed conversation already has its log in the stream, so
    // nothing opens it. The other order would put a welcome block in front of
    // every resumed conversation, every time.
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

    // **A resumed session has no welcome at all — not even the first one's.**
    //
    // That is a consequence of the design, not an accident, and it is worth stating
    // where a reader will meet it: the block is deliberately **not** a logged fact
    // (`open_conversation` writes the stream directly, so that a resume does not
    // replay it and persistence does not record it as something the session said).
    // So a resumed conversation folds a log that never had it. The alternative —
    // logging it — would put a "fact" in the log that the model never saw and that
    // a compaction would have to account for.
    for _ in 0..40 {
        s.term
            .pointer(atomcode_tui::surface::Click::WheelUp, 10, 10);
    }
    s.quiet().await;
    let top = s.screen();
    assert_eq!(
        top.matches("快速上手").count(),
        0,
        "a resumed session must not open again, and its opening was never a log \
         fact to begin with:\n{top}"
    );
    assert!(
        top.contains("remember the number 42"),
        "and the history is still all there:\n{top}"
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
        s.screen().contains("快速上手"),
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
        fresh.contains("快速上手"),
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
    assert!(screen.contains("read_file"), "the tool it used:\n{screen}");
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
async fn a_picture_pasted_for_a_blind_model_is_refused_where_it_was_typed() {
    // The failure this exists to prevent: the screen says `[Image #1]`, the log
    // records it, the encoder drops the bytes, and the model answers as if
    // nothing had been attached — with nobody having said so.
    let dir = scratch("blind-paste");
    let s = start(tree(&dir, &replay_vision(r#"{ text = "ok" }"#, false), &[])).await;
    let task = s.open().await;
    s.term.set_clipboard_image(screenshot("blind"));

    s.term.press(KeyPress::ctrl('v'));
    s.quiet().await;

    let screen = s.screen();
    assert!(
        screen.contains("看不了图片"),
        "the refusal names the model, on screen:\n{screen}"
    );
    assert!(
        !screen.contains("[Image #1]"),
        "nothing was attached, so no marker was written:\n{screen}"
    );
    // And it is refused before anything is typed: a composer holding nothing
    // cannot later send a picture nobody can see.
    s.term.type_line("看这个");
    s.quiet().await;
    assert_eq!(
        images_sent(&s),
        vec![0],
        "the turn ran with the text alone, and the log says zero images"
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
    // The turn-end caption, which is what the person reads: `✓ 完成`. Counted by
    // that caption rather than by a variant name, which the screen no longer
    // shows at all.
    let turn_ends = screen.matches("✓ 完成").count();
    assert_eq!(turn_ends, 1, "one turn, not two:\n{screen}");

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
    s.term.press(KeyPress::ctrl('t')); // fold tool calls
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

// ---- approval: the flow that makes it usable without --yolo ---------------

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

// ---- the command surface --------------------------------------------------

/// The window says which project this is.
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
    assert_eq!(
        s.term.title().as_deref(),
        Some(here),
        "the window is named after where the session is working"
    );
    task.abort();
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
    // business — `/effort` has a closed set of levels and answers with a panel
    // to pick from, which is where the second level comes from rather than from
    // the composer knowing what an argument looks like.
    let dir = scratch("menu-enter");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    // Half a name, a lit row, and one keystroke: the command runs.
    s.term.type_text("/effo");
    until(&s, "/effort").await;
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;
    let screen = s.screen();
    assert!(
        s.term.last().unwrap().part("effort").is_some(),
        "enter did not open the level panel:\n{screen}"
    );
    assert!(
        screen.contains("medium") || screen.contains("high"),
        "the panel lists the levels:\n{screen}"
    );
    // And the line was cleared rather than left holding the prefix.
    assert!(
        !screen.contains("/effo "),
        "the prefix stayed on the line behind the command that ran:\n{screen}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn tab_completes_a_command_that_takes_an_argument_and_leaves_a_space() {
    // What the registry is asked for and the label only shows: a name completed
    // without the space that says "something goes here" leaves the caret in the
    // wrong place, and the person has to type the separator the menu knew about.
    let dir = scratch("menu-takes");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[remove]]\nid = \"tui-panel-welcome\"\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_text("/effo");
    until(&s, "/effort").await;
    s.term.press(KeyPress::plain(Key::Tab));
    s.quiet().await;
    assert!(
        !s.screen().contains("思考强度"),
        "tab ran a command that wants an argument:\n{}",
        s.screen()
    );

    // Which leaves the name and a space on the line, so the argument can be
    // typed and sent the ordinary way.
    s.term.type_text("high");
    s.term.press(KeyPress::plain(Key::Enter));
    until(&s, "思考强度 → high").await;

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

    // Fold with the key, then unfold and fold again with the command. If they
    // were two implementations these two screens would differ.
    s.term.press(KeyPress::ctrl('t'));
    s.quiet().await;
    let by_key = s.screen();

    s.term.press(KeyPress::ctrl('t')); // back to open
    s.quiet().await;
    s.term.type_line("/tools");
    s.quiet().await;
    let by_command = s.screen();

    let stream_of = |screen: &str| {
        screen
            .lines()
            .filter(|l| l.contains("read_file"))
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
        by_command.contains("/tools") || by_command.contains("read_file"),
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
    assert!(panel.contains("任务"), "the header:\n{panel}");
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
        s.term.last().unwrap().rows().join("\n").contains("任务"),
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

    // The menu narrows to it as it is typed, with its hint and what it does.
    s.term.type_text("/ec");
    until(&s, "/echo <text>").await;
    assert!(s.screen().contains("say it back"), "{}", s.screen());
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
        &replay(r#"{ text = "ok" }, { text = "ok" }"#),
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

    // With no argument it asks rather than reports: the levels are a closed set
    // the command knows, so the answer is a list to pick from — and picking one
    // dispatches the command it stands for, which is the same path the typed
    // form above took.
    s.term.type_line("/effort");
    s.quiet().await;
    let panel = s.screen();
    assert!(
        s.term.last().unwrap().part("effort").is_some(),
        "the bare command opened the level panel:\n{panel}"
    );
    // The level already in force is the one marked, so the list says where the
    // session stands before anything is picked.
    assert!(
        panel.contains('●'),
        "the panel marks the level in force:\n{panel}"
    );

    // Down to the next level and take it. Which one that is depends on the
    // order of the table, so the level is read off the drawn row rather than
    // assumed — the claim is that a pick reaches the same implementation a
    // typed argument does. The row carries the frame, the cursor and the on/off
    // mark around its label, so the label is found by asking which known level
    // the row names rather than by slicing the decoration off the front.
    s.term.press(KeyPress::plain(Key::Down));
    s.quiet().await;
    let row = s
        .term
        .last()
        .unwrap()
        .part("effort")
        .unwrap()
        .lines
        .iter()
        .find(|l| l.plain().contains('▸'))
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
/// Provided by the loop and not at mount: before the loop exists there is
/// nothing to ring, and a row that took one then would hold a handle to
/// nothing.
#[tokio::test]
async fn something_working_outside_the_loop_can_ask_for_a_frame() {
    let dir = scratch("repaint");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    assert!(
        s.app
            .service::<atomcode_tui::plugin::RepaintSvc>()
            .is_none(),
        "not before it runs: there is no loop yet"
    );

    let task = s.open().await;
    let repaint = s
        .app
        .service::<atomcode_tui::plugin::RepaintSvc>()
        .expect("the loop provides it once it is running");
    repaint.now();
    assert!(
        s.term
            .settle(Duration::from_millis(40), Duration::from_secs(5))
            .await,
        "and the screen is still painting after being rung"
    );
    task.abort();
}
