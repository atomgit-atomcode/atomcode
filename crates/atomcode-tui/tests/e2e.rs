//! The whole UI, driven end to end with no tty, no network, no model and no
//! human.
//!
//! Four sources of non-determinism, all removed by construction: the model is a
//! replay fixture, the terminal is a recorder, the keyboard is a script, and
//! `settle` is a quiescence predicate that **fails** on timeout rather than
//! passing. What is left is a test that either says something true or says
//! nothing at all.

use std::sync::Arc;
use std::time::Duration;

use atomcode_harness::seams::{UiSvc, UserInterface};
use atomcode_plexus::{App, ConfigTree, Layer, PluginRegistry};
use atomcode_tui::plugin::{HeadlessSurfacePlugin, SurfaceSvc, TuiUiPlugin};
use atomcode_tui::surface::{Headless, Key, KeyPress};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("atui-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn catalog() -> PluginRegistry {
    let mut c = atomcode_harness::plugins::catalog();
    c.register(Arc::new(TuiUiPlugin))
        .register(Arc::new(HeadlessSurfacePlugin))
        .register(Arc::new(atomcode_tui::plugin::TerminalSurfacePlugin));
    for row in atomcode_tui::rows::catalog() {
        c.register(row);
    }
    c
}

/// A tree with the model scripted, the world pinned to `root`, and the screen
/// painted into memory.
fn tree(root: &std::path::Path, script: &str, extra: &[&str]) -> ConfigTree {
    let empty = root.join("__no_skills__");
    let _ = std::fs::create_dir_all(&empty);
    let base = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"mcp\"\ndisabled = true\n\n\
         [[patch]]\nid = \"tool-web\"\ndisabled = true\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
         [[patch]]\nid = \"approval\"\ndisabled = false\nconfig = {{ mode = \"yolo\" }}\n\n\
         [[patch]]\nid = \"approval-interactive\"\ndisabled = true\n\n\
         [[patch]]\nid = \"user-questions-unattended\"\ndisabled = true\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 8, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[insert]]\nid = \"surface\"\nname = \"surface-headless\"\nconfig = {{ width = 80, height = 24 }}\n\n\
         [[patch]]\nid = \"ui\"\nname = \"ui-tui2\"\n",
        root = root.to_string_lossy(),
        home = empty.to_string_lossy()
    );
    let mut layers = vec![
        atomcode_harness::bundle::base().unwrap(),
        Layer::from_toml(atomcode_harness::bundle::ONESHOT_APP).unwrap(),
        Layer::from_toml(&base).unwrap(),
        // The same screen the launcher mounts — taken from the library rather
        // than restated, so a panel that stops shipping stops being tested.
        Layer::from_toml(atomcode_tui::rows::SCREEN).unwrap(),
        Layer::from_toml(script).unwrap(),
    ];
    for e in extra {
        layers.push(Layer::from_toml(e).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

fn replay(steps: &str) -> String {
    format!("[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {steps} ] }}\n")
}

struct Session {
    /// Held, not just borrowed from: dropping the `App` unloads the whole tree,
    /// and every screen goes blank in a way that looks like a UI bug.
    _app: Arc<tokio::sync::Mutex<App>>,
    app: atomcode_plexus::Context,
    term: Arc<Headless>,
    ui: Arc<dyn UserInterface>,
}

async fn start(tree: ConfigTree) -> Session {
    let mut app = App::new(catalog(), tree);
    app.start().await.expect("the tree must mount");
    let surface = app
        .context()
        .service::<SurfaceSvc>()
        .expect("the headless surface row provides `surface`");
    let term = surface
        .as_any_headless()
        .expect("this tree mounts the headless surface");
    let ui = app.context().service::<UiSvc>().expect("`ui` is filled");
    let ctx = app.context();
    Session {
        _app: Arc::new(tokio::sync::Mutex::new(app)),
        app: ctx,
        term,
        ui,
    }
}

impl Session {
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
    /// **and** the agent says idle — and it fails on timeout rather than
    /// passing, because a `settle` that gives up quietly is a test that passes
    /// while nothing happened.
    async fn quiet(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            // Ask the agent, not the screen. Reading "idle" off the status
            // bar worked until a test hid the status bar — a predicate that
            // depends on what is *drawn* is a predicate the UI can break.
            // Settle *first*, then ask: checking busy before waiting lets a turn
            // start during the wait and still be reported quiet.
            let still = self
                .term
                .settle(Duration::from_millis(60), Duration::from_secs(5))
                .await;
            let busy = self
                .app
                .service::<atomcode_harness::seams::AgentsSvc>()
                .map(|a| {
                    a.list().iter().any(|x| {
                        // Idle with something still in the inbox is a turn that
                        // has not started yet, not a turn that has finished.
                        x.status() != atomcode_harness::agent::AgentStatus::Idle
                            || x.inbox().has_waking_input()
                    })
                })
                .unwrap_or(false);
            if still && !busy {
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
    let turn_ends = screen.matches("Stopped").count();
    assert_eq!(turn_ends, 1, "one turn, not two:\n{screen}");

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
    let s = start(tree(&dir, &replay(fail), &[wide])).await;
    // Hold the driver between "the turn ended" being committed and the agent
    // being marked idle — the window a telemetry or trace subscriber occupies
    // in a real tree, widened so the UI reliably lands inside it. A UI that
    // reads the agent's status on that fact and never looks again is caught.
    // `block_in_place`, not a bare sleep: a bare sleep pins this worker, and
    // the UI task the fact just woke sits in this worker's own run queue until
    // the hold ends — which hides the very race the hold is meant to expose.
    let _hold = s
        .app
        .on_emit::<atomcode_harness::events::TurnEnd>(|_: &atomcode_harness::seams::TurnOutcome| {
            tokio::task::block_in_place(|| std::thread::sleep(Duration::from_millis(300)));
        });
    let task = s.open().await;

    s.term.type_line("hello?");
    s.quiet().await;

    let screen = s.screen();
    assert!(screen.contains("ProviderError"), "the outcome:\n{screen}");
    assert!(
        screen.contains("nodename nor servname"),
        "the cause, wrapped rather than dropped:\n{screen}"
    );
    assert!(
        !screen.contains("运行中"),
        "the status line must not claim a finished turn is running:\n{screen}"
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
async fn without_that_row_no_key_can_conjure_the_panel() {
    // The negative control, and also the bug this refactor fixed: the toggle
    // key used to CONSTRUCT a mascot, because it had one type hard-coded. A
    // key belongs to the layout — it can show what is mounted and nothing else.
    let dir = scratch("mascot-absent");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.term.type_line("hi");
    s.quiet().await;
    assert!(s.term.last().unwrap().part("mascot").is_none());

    s.term.press(KeyPress::ctrl('n'));
    s.quiet().await;
    assert!(
        s.term.last().unwrap().part("mascot").is_none(),
        "a keystroke must not be able to mount a panel the tree did not"
    );
    let mods = s.app.service::<atomcode_tui::plugin::ModulesSvc>().unwrap();
    assert!(!mods.has_view("mascot"), "and the registry is untouched");

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_panel_can_be_hidden_and_brought_back_while_the_session_runs() {
    // Visibility, the other axis: the panel stays mounted throughout, so this
    // is the layout changing and not the tree.
    let dir = scratch("mascot-toggle");
    let s = start(tree(
        &dir,
        &replay(r#"{ text = "ok" }"#),
        &["[[patch]]\nid = \"tui-panel-mascot\"\ndisabled = false"],
    ))
    .await;
    let task = s.open().await;
    s.term.type_line("hi");
    s.quiet().await;
    let mods = s.app.service::<atomcode_tui::plugin::ModulesSvc>().unwrap();
    assert!(s.term.last().unwrap().part("mascot").is_some());

    s.term.press(KeyPress::ctrl('n'));
    s.quiet().await;
    assert!(s.term.last().unwrap().part("mascot").is_none(), "hidden");
    assert!(
        mods.has_view("mascot"),
        "but still mounted — hiding is not unmounting"
    );

    s.term.press(KeyPress::ctrl('n'));
    s.quiet().await;
    assert!(s.term.last().unwrap().part("mascot").is_some(), "and back");

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
    assert!(s.screen().contains("Cancelled"), "{}", s.screen());

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
fn asking(root: &std::path::Path, script: &str) -> ConfigTree {
    tree(
        root,
        script,
        &[
            "[[patch]]\nid = \"approval\"\ndisabled = true\n",
            "[[patch]]\nid = \"approval-interactive\"\ndisabled = false\n",
        ],
    )
}

#[tokio::test]
async fn a_risky_call_is_asked_about_on_screen_and_yes_lets_it_run() {
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
    let asked = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if s.screen().contains("write_file") && s.screen().contains("esc)") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(asked.is_ok(), "no question appeared:\n{}", s.screen());
    assert!(
        !dir.join("out.txt").exists(),
        "nothing may run before it is approved"
    );

    s.term.press(KeyPress::ch('y'));
    s.quiet().await;
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "written",
        "an approved call runs"
    );
    let screen = s.screen();
    assert!(
        screen.contains("→ yes"),
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
    let asked = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if s.screen().contains("esc)") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(asked.is_ok(), "no question appeared:\n{}", s.screen());

    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;

    assert!(!dir.join("out.txt").exists(), "a refused call must not run");
    let screen = s.screen();
    // Refusing is *returning a result*: the model learns why and the turn
    // finishes, instead of dying with a dangling call.
    assert!(
        screen.contains("declined"),
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

#[tokio::test]
async fn typing_a_slash_shows_what_is_available_and_narrows_as_you_type() {
    let dir = scratch("menu");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_text("/");
    s.quiet().await;
    let all = s.screen();
    assert!(all.contains("/help"), "the menu opens:\n{all}");
    assert!(all.contains("/compact"), "{all}");

    s.term.type_text("comp");
    s.quiet().await;
    let narrowed = s.screen();
    assert!(narrowed.contains("/compact"), "{narrowed}");
    assert!(!narrowed.contains("/help"), "it narrows:\n{narrowed}");

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

    s.term.type_line("/comp");
    s.quiet().await;
    let screen = s.screen();
    assert!(screen.contains("/compact"), "it suggests:\n{screen}");

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

/// A tree with `control` mounted, so `/rows` has something to show.
async fn with_control(tree: ConfigTree) -> (Session, Arc<tokio::sync::Mutex<App>>) {
    let mut app = App::new(catalog(), tree);
    app.start().await.expect("the tree must mount");
    let surface = app.context().service::<SurfaceSvc>().unwrap();
    let term = surface.as_any_headless().unwrap();
    let ui = app.context().service::<UiSvc>().unwrap();
    let ctx = app.context();
    let app = Arc::new(tokio::sync::Mutex::new(app));
    let _ = ctx.provide::<atomcode_harness::seams::ControlSvc>(Arc::new(
        atomcode_harness::control::AppControl::new(app.clone()),
    ));
    (
        Session {
            _app: app.clone(),
            app: ctx,
            term,
            ui,
        },
        app,
    )
}

#[tokio::test]
async fn a_modal_takes_the_keyboard_and_escape_gives_it_back() {
    let dir = scratch("modal");
    let (s, _keep) = with_control(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    s.term.type_line("/rows");
    s.quiet().await;
    let open = s.screen();
    assert!(open.contains("enter 开关"), "the modal is framed:\n{open}");
    assert!(
        open.contains("agent-loop") || open.contains("llm"),
        "{open}"
    );

    // Typing goes to the modal's filter, not to the prompt.
    s.term.type_text("llm");
    s.quiet().await;
    let filtered = s.screen();
    assert!(filtered.contains("llm"), "{filtered}");
    assert!(
        !filtered.contains("❯ llm"),
        "the keys went to the modal, not the prompt:\n{filtered}"
    );

    s.term.press(KeyPress::plain(Key::Esc));
    s.quiet().await;
    let closed = s.screen();
    assert!(!closed.contains("enter 开关"), "closed:\n{closed}");

    // And the prompt has the keyboard back.
    s.term.type_text("hello");
    s.quiet().await;
    let prompt = atomcode_tui::caps::Caps::default().g(atomcode_tui::caps::Glyph::Prompt);
    assert!(
        s.screen().contains(&format!("{prompt} hello")),
        "{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn picking_a_row_reconfigures_the_running_tree() {
    let dir = scratch("swap");
    let (s, _keep) = with_control(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;

    // The tool catalog before.
    s.term.type_line("/tools-list");
    s.quiet().await;
    assert!(s.screen().contains("read_file"), "{}", s.screen());

    // Turn off the row that provides the filesystem tools, from the modal.
    s.term.type_line("/rows");
    s.quiet().await;
    s.term.type_text("tool-fs-world");
    s.quiet().await;
    s.term.press(KeyPress::plain(Key::Enter));
    s.quiet().await;

    s.term.type_line("/tools-list");
    s.quiet().await;
    let after = s.screen();
    // The earlier listing is still scrolled above, so assert on the *last*
    // answer rather than on the whole screen — a scrollback that still says
    // `read_file` is the transcript doing its job.
    let latest = after
        .lines()
        .rfind(|l| l.contains("bash") && l.contains("grep"))
        .unwrap_or_else(|| panic!("no tool listing on screen:\n{after}"));
    assert!(
        !latest.contains("read_file"),
        "the tree really changed under a running session:\n{after}"
    );
    assert!(latest.contains("bash"), "and only that row went:\n{after}");

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// ---- runtime layout -------------------------------------------------------

#[tokio::test]
async fn the_screen_can_be_rearranged_while_it_runs_and_put_back() {
    let dir = scratch("layout");
    let s = start(tree(&dir, &replay(r#"{ text = "ok" }"#), &[])).await;
    let task = s.open().await;
    s.term.type_line("hi");
    s.quiet().await;
    assert!(
        s.term.last().unwrap().part("status").is_some(),
        "the status line is up"
    );

    // By command.
    s.term.type_line("/hide status");
    s.quiet().await;
    assert!(
        s.term.last().unwrap().part("status").is_none(),
        "hidden:\n{}",
        s.screen()
    );

    // By key — the same `apply`.
    s.term.press(KeyPress::ctrl('z'));
    s.quiet().await;
    assert!(
        s.term.last().unwrap().part("status").is_some(),
        "undone by ctrl-z:\n{}",
        s.screen()
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn the_model_is_told_the_layout_and_can_change_it() {
    let dir = scratch("model-layout");
    // The model calls the tool it was told about in its prompt.
    let script = replay(
        r#"{ text = "收起来。", calls = [ { name = "adjust_layout", args = { op = "hide", module = "status" } } ] },
           { text = "好了。" }"#,
    );
    let s = start(tree(
        &dir,
        &script,
        &["[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }\n"],
    ))
    .await;
    let task = s.open().await;

    // What the model is told comes from the same tree the screen draws.
    let prompt = s
        .app
        .service::<atomcode_harness::seams::SystemPromptSvc>()
        .expect("the prompt registry is mounted")
        .render();
    assert!(prompt.contains("屏幕布局"), "the model is told:\n{prompt}");
    assert!(prompt.contains("adjust_layout"), "{prompt}");

    s.term.type_line("把状态栏收起来");
    s.quiet().await;
    let after = s.screen();
    assert!(
        !after.contains("atomcode ·"),
        "the model rearranged the screen:\n{after}"
    );
    assert!(after.contains("好了"), "and the turn finished:\n{after}");

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test]
async fn a_layout_request_the_model_gets_wrong_comes_back_with_the_reason() {
    let dir = scratch("model-layout-bad");
    let script = replay(
        r#"{ text = "试试。", calls = [ { name = "adjust_layout", args = { op = "hide", module = "nonesuch" } } ] },
           { text = "明白了。" }"#,
    );
    let s = start(tree(
        &dir,
        &script,
        &["[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }\n"],
    ))
    .await;
    let task = s.open().await;

    s.term.type_line("收起 nonesuch");
    s.quiet().await;
    let after = s.screen();
    assert!(
        after.contains("adjust_layout"),
        "the call is on screen:\n{after}"
    );
    assert!(
        after.contains("nonesuch") || after.contains("不在屏幕上"),
        "with a reason the model can act on:\n{after}"
    );
    assert!(
        after.contains("明白了"),
        "and the turn carried on:\n{after}"
    );

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

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
         [[patch]]\nid = \"tui-panel-team\"\ndisabled = false\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\n\
         config = {{ script = [ \
           {{ text = \"MEMBER-THINKING-OUT-LOUD\", calls = [ {{ name = \"tell_parent\", args = {{ text = \"sessions are made in agent.rs\" }} }} ] }}, \
           {{ text = \"MEMBER-TRAILING-WORDS\" }} ] }}\n",
        dir = dir.to_string_lossy(),
    );
    let s = start(tree(&dir, &script, &[&team])).await;
    let task = s.open().await;

    // Before anyone is delegated to, the panel is one line and says so.
    let empty = s
        .term
        .last()
        .unwrap()
        .part("team")
        .expect("the team panel is on screen")
        .clone();
    assert_eq!(empty.rect.h, 1, "an empty team takes one line");

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

    s.term.press(KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}
