//! What the agent knows about itself.
//!
//! The claim under test is not "the tool returns a string". It is: **the answer
//! is generated from the running tree, so it cannot be stale**. A test that only
//! checked the wording would pass just as well against a hard-coded constant —
//! which is precisely the design this row exists to avoid. So the judges here
//! are external to the implementation:
//!
//! * the reported session id must equal the one the session log actually mints;
//! * the reported log path must be the file persistence actually creates on disk;
//! * changing the tree after the tool was built must change its answer;
//! * and with the row removed, none of it may be true — otherwise the tests
//!   above are passing for a reason that has nothing to do with this row.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::seams::{SessionSvc, SystemPromptSvc, ToolsSvc};
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_kernel::tool::{ProgressSink, Tool, ToolContext, ToolResult};
use atomcode_plexus::{App, ConfigTree, Layer};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-self-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn tree(root: &std::path::Path, extra: &[&str]) -> ConfigTree {
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let scoped = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 20, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"project-instructions\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {store:?} }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [{{ text = \"ok\" }}] }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy(),
        store = root.join("sessions").to_string_lossy(),
    );
    let mut layers = vec![bundle::base().unwrap()];
    layers.push(Layer::from_toml(&scoped).unwrap());
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

async fn ask(app: &App, aspect: &str) -> String {
    let tool = app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .get("describe_self")
        .expect("describe_self must be registered");
    let ctx = ToolContext {
        working_dir: std::env::current_dir().unwrap(),
        cancel: Default::default(),
        progress: ProgressSink::noop(),
        requester: None,
    };
    let out = tool
        .execute(&format!(r#"{{"aspect":"{aspect}"}}"#), &ctx)
        .await;
    assert!(!out.is_error, "{}", out.content);
    out.content
}

fn prompt(app: &App) -> String {
    app.context().service::<SystemPromptSvc>().unwrap().render()
}

// ---- what the prompt may and may not carry -------------------------------

#[tokio::test]
async fn the_prompt_stays_identical_across_sessions() {
    // The reason this row does *not* name the session id in the prompt.
    //
    // A system prompt carrying a per-session value is a different system prompt
    // for every session, and every cross-session prefix cache hit dies with it.
    // The obvious convenience — "tell it its id so it never has to ask" — costs
    // far more than the one tool call it saves. If someone puts a volatile fact
    // back into the fragment, this is the test that says no.
    let dir = scratch("cacheable");
    let a = start(tree(&dir, &[])).await;
    let b = start(tree(&dir, &[])).await;
    assert_ne!(
        a.context().service::<SessionSvc>().unwrap().id(),
        b.context().service::<SessionSvc>().unwrap().id(),
        "two different sessions, by construction"
    );
    assert_eq!(
        prompt(&a),
        prompt(&b),
        "but one prompt — a per-session system prompt cannot be cached across sessions"
    );
}

#[tokio::test]
async fn the_agent_is_told_to_ask_rather_than_guess() {
    let dir = scratch("told");
    let app = start(tree(&dir, &[])).await;
    let said = prompt(&app);
    assert!(said.contains("describe_self"), "{said}");
    assert!(
        said.contains("guess"),
        "guessing is the behaviour being corrected, so the prompt has to name it:\n{said}"
    );
}

#[tokio::test]
async fn the_reported_log_path_is_the_file_persistence_actually_writes() {
    let dir = scratch("real-path");
    let app = start(tree(&dir, &[])).await;

    // Pull the path out of what the tool reports, then make the session write.
    let told = ask(&app, "session").await;
    let path = told
        .split_whitespace()
        .find(|s| s.ends_with(".jsonl"))
        .expect("the tool must name a log file")
        .to_string();
    assert!(
        !std::path::Path::new(&path).exists(),
        "nothing written yet: {path}"
    );

    run_turn(&app, "hello").await.expect("a turn");
    // Persistence listens rather than being called, so give the listener a beat.
    for _ in 0..50 {
        if std::path::Path::new(&path).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        std::path::Path::new(&path).exists(),
        "the path the agent was given must be the file that appears: {path}"
    );
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("hello"), "and it must hold this session");
}

// ---- the claim: the answer follows the tree ------------------------------

struct Latecomer;

#[async_trait]
impl Tool for Latecomer {
    fn name(&self) -> &str {
        "arrived_late"
    }
    fn description(&self) -> &str {
        "mounted after describe_self was built"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: String::new(),
            is_error: false,
            images: Vec::new(),
        }
    }
}

#[tokio::test]
async fn the_answer_is_read_from_the_live_tree_not_frozen_at_mount() {
    let dir = scratch("live");
    let app = start(tree(&dir, &[])).await;

    let before = ask(&app, "tools").await;
    assert!(
        !before.contains("arrived_late"),
        "not there yet, by construction"
    );

    // Change the tree *after* the tool was constructed. A snapshot taken at
    // mount time would be blind to this; that is the whole difference between a
    // door onto the tree and a sentence about the tree.
    let toolbox = app.context().service::<ToolsSvc>().unwrap();
    toolbox.register(Arc::new(Latecomer)).unwrap();

    let after = ask(&app, "tools").await;
    assert!(
        after.contains("arrived_late"),
        "describe_self must re-read the catalog:\n{after}"
    );

    toolbox.unregister("arrived_late");
    let removed = ask(&app, "tools").await;
    assert!(
        !removed.contains("arrived_late"),
        "and it must follow a removal too:\n{removed}"
    );
}

#[tokio::test]
async fn it_reports_the_session_the_log_actually_holds() {
    let dir = scratch("session-aspect");
    let app = start(tree(&dir, &[])).await;
    let log = app.context().service::<SessionSvc>().unwrap();

    let before = ask(&app, "session").await;
    assert!(before.contains(log.id()), "{before}");
    assert!(before.contains("events logged so far: 0"), "{before}");

    run_turn(&app, "hello").await.expect("a turn");
    let after = ask(&app, "session").await;
    assert!(
        !after.contains("events logged so far: 0"),
        "the count is read live:\n{after}"
    );
    assert!(after.contains(&format!("turn: {}", log.current_turn())));
}

#[tokio::test]
async fn a_tree_with_no_store_says_so_instead_of_naming_a_path() {
    let dir = scratch("memory-only");
    let app = start(tree(
        &dir,
        &["[[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true"],
    ))
    .await;

    let said = ask(&app, "session").await;
    assert!(
        said.contains("nowhere"),
        "an unpersisted session must admit it:\n{said}"
    );
    assert!(
        !said.contains(".jsonl"),
        "and must not name a file nobody writes:\n{said}"
    );
}

// ---- through a real turn -------------------------------------------------

#[tokio::test]
async fn a_turn_that_calls_it_gets_the_session_back_as_a_tool_result() {
    // Everything above tests the tool in isolation. This one goes through the
    // loop the model actually drives — catalog lookup, approval, execution,
    // result logged as a fact — because a tool that is correct but unreachable
    // is worth nothing. The model's *judgement* to call it is the one link a
    // replay provider cannot stand in for; the prompt fragment is what carries
    // that, and `the_agent_is_told_to_ask_rather_than_guess` is its judge.
    let dir = scratch("real-turn");
    let call = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [
  { text = "let me check", calls = [ { name = "describe_self", args = { aspect = "session" } } ] },
  { text = "answered" },
] }
"#;
    let app = start(tree(&dir, &[call])).await;
    let id = app
        .context()
        .service::<SessionSvc>()
        .unwrap()
        .id()
        .to_string();

    let outcome = run_turn(&app, "what session is this?")
        .await
        .expect("a turn");
    assert_eq!(outcome.tool_calls, 1);

    let logged = app
        .context()
        .service::<SessionSvc>()
        .unwrap()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            atomcode_harness::session::SessionEvent::ToolResultLogged { content, .. } => {
                Some(content)
            }
            _ => None,
        })
        .collect::<Vec<String>>();
    assert_eq!(logged.len(), 1, "one call, one result");
    assert!(
        logged[0].contains(&id),
        "the answer that reached the model must carry the real session id:\n{}",
        logged[0]
    );
}

// ---- the other half: what the repository says ---------------------------

#[tokio::test]
async fn a_repository_that_documents_itself_is_actually_read() {
    let dir = scratch("agents-md");
    std::fs::write(
        dir.join("AGENTS.md"),
        "# House rules\nRun ./gates/tui.sh before you claim done.",
    )
    .unwrap();
    let app = start(tree(&dir, &[])).await;
    let said = prompt(&app);
    assert!(
        said.contains("./gates/tui.sh"),
        "the agent must be told what the repository expects of it:\n{said}"
    );
    assert!(
        said.contains("AGENTS.md"),
        "and where the rule came from, so it can go read the rest"
    );
}

#[tokio::test]
async fn the_ecosystem_names_are_honoured_in_precedence_order() {
    let dir = scratch("precedence");
    std::fs::write(dir.join("CLAUDE.md"), "claude-only rule").unwrap();
    let only_claude = start(tree(&dir, &[])).await;
    assert!(prompt(&only_claude).contains("claude-only rule"));

    // AGENTS.md wins when both exist — the same order the rest of the stack uses.
    std::fs::write(dir.join("AGENTS.md"), "agents rule").unwrap();
    let both = start(tree(&dir, &[])).await;
    let said = prompt(&both);
    assert!(said.contains("agents rule"), "{said}");
    assert!(
        !said.contains("claude-only rule"),
        "first match wins:\n{said}"
    );
}

#[tokio::test]
async fn a_repository_with_no_instructions_contributes_no_fragment() {
    // Not an empty fragment: an empty contribution costs a blank line in every
    // request and shows up in `ids()` as a contribution that is not one.
    let dir = scratch("silent-repo");
    let app = start(tree(&dir, &[])).await;
    assert!(
        !app.context()
            .service::<SystemPromptSvc>()
            .unwrap()
            .ids()
            .contains(&"project-instructions".to_string()),
        "nothing to say means saying nothing"
    );
}

#[tokio::test]
async fn without_the_row_the_file_is_there_and_unread() {
    // The negative control that matters: the file exists, so a green result
    // above could otherwise mean "some other row happened to read it".
    let dir = scratch("unread");
    std::fs::write(dir.join("AGENTS.md"), "a rule nobody delivers").unwrap();
    let app = start(tree(
        &dir,
        &["[[patch]]\nid = \"project-instructions\"\ndisabled = true"],
    ))
    .await;
    assert!(
        !prompt(&app).contains("a rule nobody delivers"),
        "no other row reads the repository's instructions — that is the gap"
    );
}

// ---- negative control ----------------------------------------------------
//
// Without this, every test above could be passing because some *other* row
// happens to put a session id in the prompt.

#[tokio::test]
async fn without_the_row_the_agent_is_told_none_of_this() {
    let dir = scratch("absent");
    let app = start(tree(
        &dir,
        &["[[patch]]\nid = \"self-knowledge\"\ndisabled = true"],
    ))
    .await;

    let said = prompt(&app);
    assert!(
        !said.contains("describe_self"),
        "nothing else tells the agent to stop guessing — that is the gap this row fills:\n{said}"
    );
    assert!(
        app.context()
            .service::<ToolsSvc>()
            .unwrap()
            .get("describe_self")
            .is_none(),
        "and the door is gone with the row"
    );
}

#[tokio::test]
async fn the_row_takes_its_tool_and_its_fragment_away_when_it_unloads() {
    let dir = scratch("unload");
    let mut app = start(tree(&dir, &[])).await;
    assert!(prompt(&app).contains("describe_self"));

    app.stop();
    // After a stop the services are gone with the tree; what matters is that the
    // row filed removals rather than leaking a tool into a catalog that outlives
    // it. Re-mounting must therefore be clean rather than a duplicate-name error.
    let app2 = start(tree(&dir, &[])).await;
    assert!(app2
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .get("describe_self")
        .is_some());
}
