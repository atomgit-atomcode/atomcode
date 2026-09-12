//! Two things an agent could not do until now: ask, and keep its own plan true.
//!
//! Both are about the same failure — an agent that works in silence and reports
//! at the end. It could not ask, because nothing mounted a tool for it and the
//! tool context's requester is `None`; and its task list went stale mid-turn,
//! because the instruction to update it lives in a tool description twenty
//! thousand tokens from the step being taken.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::agent::OnlySession;
use atomcode_harness::seams::{Question, UserQuestions, UserQuestionsSvc};
use atomcode_harness::session::{InjectionOrigin, SessionEvent};
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
    let mut layers = vec![bundle::base().unwrap()];
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

/// Everything the harness told the model without the person saying it.
fn reminders(app: &App) -> Vec<String> {
    facts(app)
        .into_iter()
        .filter_map(|f| match f {
            SessionEvent::Injected {
                text,
                origin: InjectionOrigin::Reminder,
                ..
            } => Some(text),
            _ => None,
        })
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

// ---- the list ------------------------------------------------------------

/// Plan two tasks, then work for three steps without touching the list.
///
/// `list_directory` is the filler on purpose: it is safe, it always succeeds,
/// and it is exactly the shape of the work that makes a model forget — read
/// something, read something else, read something else.
const FORGETS: &str = r#"{ text = "Planning.", calls = [ { name = "todowrite", args = { todos = [ { content = "read the parser", status = "in_progress" }, { content = "fix the parser", status = "pending" } ] } } ] },
   { text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
   { text = "Still looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
   { text = "And again.", calls = [ { name = "list_directory", args = { path = "." } } ] },
   { text = "Done." }"#;

#[tokio::test]
async fn a_list_that_stopped_describing_the_work_is_said_so_once() {
    let dir = scratch("stale");
    let after_two = "[[patch]]\nid = \"todo-reminder\"\nconfig = { after_steps = 2 }";
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, &replay(FORGETS), &[after_two]),
    );
    app.start().await.unwrap();
    run_turn(&app, "fix the parser").await.unwrap();

    let said = reminders(&app);
    assert!(
        !said.is_empty(),
        "three steps of silence with a task in progress is stale"
    );
    assert!(
        said[0].contains("read the parser"),
        "it names the task the list still claims: {}",
        said[0]
    );
    assert!(
        said[0].contains("<system-reminder>") && said[0].contains("Do not mention"),
        "and it is machinery, not something to read aloud: {}",
        said[0]
    );
    // Spacing, not silence: a reminder every step is noise, and noise is what a
    // model learns to skip.
    assert!(
        said.len() <= 2,
        "one reminder per stretch of silence, not one per step: {said:?}"
    );
}

#[tokio::test]
async fn a_list_kept_up_to_date_is_never_mentioned() {
    // The same work, with the model doing what the tool asked of it. Nothing
    // about this turn is worth a sentence.
    let keeps_up = r#"{ text = "Planning.", calls = [ { name = "todowrite", args = { todos = [ { content = "read the parser", status = "in_progress" } ] } } ] },
       { text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
       { text = "Done that.", calls = [ { name = "todowrite", args = { action = "update", id = 1, status = "completed" } } ] },
       { text = "Finished." }"#;
    let dir = scratch("tidy");
    let after_two = "[[patch]]\nid = \"todo-reminder\"\nconfig = { after_steps = 2 }";
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, &replay(keeps_up), &[after_two]),
    );
    app.start().await.unwrap();
    run_turn(&app, "fix the parser").await.unwrap();

    assert!(
        reminders(&app).is_empty(),
        "a list that is true says nothing: {:?}",
        reminders(&app)
    );
}

#[tokio::test]
async fn a_session_that_never_planned_is_left_alone() {
    // The row must not become a nag about using `todowrite` at all. A one-line
    // fix does not need a plan, and the tool says so itself.
    let dir = scratch("no-plan");
    let never = r#"{ text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
       { text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
       { text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
       { text = "Done." }"#;
    let after_two = "[[patch]]\nid = \"todo-reminder\"\nconfig = { after_steps = 2 }";
    let mut app = App::new(plugins::catalog(), tree(&dir, &replay(never), &[after_two]));
    app.start().await.unwrap();
    run_turn(&app, "look around").await.unwrap();
    assert!(reminders(&app).is_empty(), "{:?}", reminders(&app));
}
