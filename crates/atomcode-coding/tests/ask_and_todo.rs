//! Something an agent could not do until now: ask.
//!
//! It could not, because nothing mounted a tool for it and the tool context's
//! requester is `None` — so an agent that needed a decision worked in silence
//! and reported at the end.
//!
//! The other half this file used to hold — the task list going stale mid-turn —
//! is coding's `TodoHook` now (`src/todo.rs` states its criteria); coding keeps
//! the harness's `todo-reminder` row off, and that row's own criteria live with
//! it in `atomcode-harness/tests/capabilities.rs`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::agent::OnlySession;
use atomcode_harness::seams::{Question, UserQuestions, UserQuestionsSvc};
use atomcode_harness::session::SessionEvent;
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin};
use serde_json::Value;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-ask-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn tree(root: &std::path::Path, script: &str, extra: &[&str]) -> ConfigTree {
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let scoped = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 20, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
         [[patch]]\nid = \"approval\"\nconfig = {{ mode = \"yolo\" }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut layers = vec![
        atomcode_coding::on_harness::base_layer(),
        atomcode_coding::on_harness::headless_patch(),
    ];
    for src in [script, quiet, scoped.as_str()] {
        layers.push(Layer::from_toml(src).unwrap());
    }
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

fn replay(steps: &str) -> String {
    format!("[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {steps} ] }}\n")
}

fn facts(app: &App) -> Vec<SessionEvent> {
    app.context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .map(|e| e.event)
        .collect()
}

fn tool_results(app: &App) -> Vec<String> {
    facts(app)
        .into_iter()
        .filter_map(|f| match f {
            SessionEvent::ToolResultLogged { content, .. } => Some(content),
            _ => None,
        })
        .collect()
}

// ---- asking -------------------------------------------------------------

/// A person who always takes the option named here, and records the question.
struct Picks(&'static str, Arc<Mutex<Vec<Question>>>);

#[async_trait]
impl UserQuestions for Picks {
    fn describe(&self) -> String {
        format!("scripted human (always picks `{}`)", self.0)
    }
    async fn ask(&self, question: &Question) -> Option<String> {
        self.1.lock().unwrap().push(question.clone());
        question
            .options
            .iter()
            .find(|a| a.value == self.0)
            .map(|a| a.value.clone())
    }
}

struct PicksPlugin(&'static str, Arc<Mutex<Vec<Question>>>);

#[async_trait]
impl Plugin for PicksPlugin {
    fn name(&self) -> &'static str {
        "user-questions-scripted"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["user-questions"]
    }
    fn description(&self) -> &'static str {
        "a scripted human, for tests"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UserQuestionsSvc>(Arc::new(Picks(self.0, self.1.clone())))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

const ASKS: &str = r#"{ text = "I need to know.", calls = [ { name = "ask_user", args = { question = "Add a [telemetry] section, or leave it off?", options = [ "add it", "leave it off" ] } } ] }, { text = "Left it off, as asked." }"#;

#[tokio::test]
async fn the_agent_can_put_a_choice_to_the_person_and_hears_the_answer() {
    let dir = scratch("asks");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut registry = plugins::catalog();
    registry.register(Arc::new(PicksPlugin("leave it off", asked.clone())));
    // Replacing the dormant fail-closed row rather than standing beside it:
    // one provider per seam, and in a tree with no front end that row is the
    // only slot a scripted human can take.
    let swap = "[[patch]]\nid = \"user-questions-unattended\"\n\
                name = \"user-questions-scripted\"\ndisabled = false";
    let mut app = App::new(registry, tree(&dir, &replay(ASKS), &[swap]));
    app.start().await.unwrap();
    let out = run_turn(&app, "set telemetry up").await.unwrap();

    let seen = asked.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "asked once: {seen:?}");
    assert!(seen[0].prompt.contains("telemetry"), "{:?}", seen[0].prompt);
    assert_eq!(seen[0].values(), vec!["add it", "leave it off"]);
    assert!(
        seen[0].about.is_none(),
        "a question the model asked is not an approval about a call"
    );

    let results = tool_results(&app);
    assert!(
        results.iter().any(|r| r.contains("leave it off")),
        "the answer comes back as the tool's result: {results:?}"
    );
    assert_eq!(out.text, "Left it off, as asked.");

    // And the exchange is a fact, not just a screen that scrolled by. Both
    // halves have to be here: without the question a client connecting midway
    // cannot see what is waiting, and without the answer nobody can tell it was
    // ever decided.
    let asked: Vec<_> = facts(&app)
        .into_iter()
        .filter_map(|f| match f {
            SessionEvent::Asked { question, .. } => Some(question),
            _ => None,
        })
        .collect();
    assert_eq!(asked.len(), 1, "asked once, logged once: {asked:?}");
    assert_eq!(
        asked[0].values(),
        vec!["add it", "leave it off"],
        "the options are on the card, not only in the seam's return value"
    );
    let answered: Vec<_> = facts(&app)
        .into_iter()
        .filter_map(|f| match f {
            SessionEvent::Answered { answer, by, .. } => Some((answer, by)),
            _ => None,
        })
        .collect();
    assert_eq!(answered.len(), 1, "closed once: {answered:?}");
    assert_eq!(
        answered[0].0.as_deref(),
        Some("leave it off"),
        "and what the person picked"
    );
    assert_eq!(
        answered[0].1, "scripted human (always picks `leave it off`)",
        "with the front end's own name on it — who answered is part of the record"
    );
}

#[tokio::test]
async fn with_nobody_to_ask_the_turn_carries_on_instead_of_stopping() {
    // The default tree answers every question with "no": an agent that stopped
    // dead there would be worse than one that never asked, because it would
    // have spent a round to get stuck.
    let dir = scratch("nobody");
    let mut app = App::new(plugins::catalog(), tree(&dir, &replay(ASKS), &[]));
    app.start().await.unwrap();
    let out = run_turn(&app, "set telemetry up").await.unwrap();

    let results = tool_results(&app);
    assert!(
        results.iter().any(|r| r.contains("not agreement")),
        "a non-answer is not consent, and it says so: {results:?}"
    );
    assert!(
        !results
            .iter()
            .any(|r| r.contains("Interactive questions are not supported")),
        "and it is answered by the seam, not refused by a missing requester: {results:?}"
    );
    assert_eq!(out.text, "Left it off, as asked.", "the turn finished");
}
