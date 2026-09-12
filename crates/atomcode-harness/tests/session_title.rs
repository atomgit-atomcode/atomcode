//! Naming a session: when it happens, who answers, and who wins.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use atomcode_harness::session::SessionEvent;
use atomcode_harness::{bundle, create_agent, plugins, run_turn};
use atomcode_plexus::{App, ConfigTree, Layer};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-title-{}-{tag}-{n}", std::process::id()));
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
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n",
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

fn talker(steps: &[&str]) -> String {
    let list = steps
        .iter()
        .map(|t| format!("{{ text = {t:?} }}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {list} ] }}")
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

/// The title is asked for in the background; give it a moment.
async fn titled(agent: &atomcode_harness::agent::Agent) -> Option<String> {
    for _ in 0..100 {
        if let Some(t) = agent.session().title() {
            return Some(t);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

fn titles(agent: &atomcode_harness::agent::Agent) -> Vec<String> {
    agent
        .session()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Titled { title, .. } => Some(title),
            _ => None,
        })
        .collect()
}

const MODEL_NAMER: &str =
    "[[patch]]\nid = \"session-title-first-prompt\"\nname = \"session-title-model\"";

#[tokio::test]
async fn the_first_prompt_names_the_session_once() {
    let dir = scratch("first");
    let app = start(tree(&dir, &talker(&["ok", "ok"]), &[])).await;
    let agent = create_agent(&app).await.unwrap();
    run_turn(
        &app,
        "make the build stop failing on windows please, it is urgent",
    )
    .await
    .unwrap();
    assert_eq!(
        titled(&agent).await.as_deref(),
        Some("make the build stop failing on windows please"),
        "eight words of the first prompt, logged as a fact"
    );
    run_turn(&app, "and then update the changelog")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(titles(&agent).len(), 1, "named once, not once per prompt");
}

#[tokio::test]
async fn the_utility_model_names_it_when_the_row_says_so() {
    let dir = scratch("model");
    // The conversation's script and the namer's script are different rows, so
    // the title comes from the utility model and the conversation keeps its
    // own answers.
    let utility = "[[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\n\
                   config = { script = [ { text = \"\\\"Windows build fix.\\\"\" } ] }";
    let app = start(tree(
        &dir,
        &talker(&["the answer", "the answer"]),
        &[MODEL_NAMER, utility],
    ))
    .await;
    let agent = create_agent(&app).await.unwrap();
    let outcome = run_turn(&app, "make the build stop failing on windows please")
        .await
        .unwrap();
    assert_eq!(
        outcome.text, "the answer",
        "the conversation's script was not eaten"
    );
    assert_eq!(
        titled(&agent).await.as_deref(),
        Some("Windows build fix"),
        "quotes and the trailing period are tidied away"
    );
}

#[tokio::test]
async fn without_a_utility_model_the_namer_falls_back_to_the_first_prompt() {
    let dir = scratch("fallback");
    // `session-title-model` mounted, no `llm-utility` row: it must not borrow
    // the conversation's model — that would eat the script's next line.
    let app = start(tree(&dir, &talker(&["only one answer"]), &[MODEL_NAMER])).await;
    let agent = create_agent(&app).await.unwrap();
    let outcome = run_turn(&app, "what is this repository").await.unwrap();
    assert_eq!(outcome.text, "only one answer");
    assert_eq!(
        titled(&agent).await.as_deref(),
        Some("what is this repository")
    );
}

#[tokio::test]
async fn a_name_the_person_gave_is_kept() {
    let dir = scratch("kept");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let agent = create_agent(&app).await.unwrap();
    atomcode_harness::session::commit(
        &app.context(),
        &agent.session(),
        SessionEvent::Titled {
            turn: 0,
            title: "mine".into(),
        },
    );
    run_turn(&app, "something else entirely").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(agent.session().title().as_deref(), Some("mine"));
    assert_eq!(titles(&agent), vec!["mine".to_string()]);
}

#[tokio::test]
async fn removing_the_policy_row_leaves_sessions_unnamed() {
    let dir = scratch("no-policy");
    let app = start(tree(
        &dir,
        &talker(&["ok"]),
        &["[[remove]]\nid = \"session-title-on-first-prompt\""],
    ))
    .await;
    let agent = create_agent(&app).await.unwrap();
    run_turn(&app, "hello").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(agent.session().title().is_none());
}
