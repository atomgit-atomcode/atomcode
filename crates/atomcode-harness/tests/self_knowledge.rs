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

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::seams::{LlmSvc, SystemPromptSvc, ToolsSvc};
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
    // The tree's own agent, so the session exists before the first turn — what
    // the `session` row used to do at mount.
    atomcode_harness::create_agent(&app)
        .await
        .expect("an agent");
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
        a.context().only_session().unwrap().id(),
        b.context().only_session().unwrap().id(),
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
async fn only_the_persona_row_says_who_the_agent_is() {
    // Both fragments reach the model in one request, so two rows opening with
    // "you are" is two answers to one question and the loser is whichever the
    // model reads second. Identity is the persona row's sentence; this row
    // states what the agent is assembled from and never who it is.
    //
    // The judge is the rendered prompt rather than either constant, because the
    // rendered prompt is the only thing the model actually sees. An identity
    // claim is a paragraph that *opens* with it — "read the code you are about
    // to change" is prose, not a second answer to "who are you".
    fn identity_claims(prompt: &str) -> Vec<&str> {
        prompt
            .split("\n\n")
            .filter(|p| p.trim_start().to_lowercase().starts_with("you are"))
            .collect()
    }

    let dir = scratch("identity");

    let with_persona = prompt(&start(tree(&dir, &[])).await);
    assert_eq!(
        identity_claims(&with_persona).len(),
        1,
        "exactly one row may claim an identity, got {:?}",
        identity_claims(&with_persona)
    );

    let without_persona = prompt(
        &start(tree(
            &dir,
            &["[[patch]]\nid = \"persona-coding\"\ndisabled = true"],
        ))
        .await,
    );
    assert!(
        identity_claims(&without_persona).is_empty(),
        "with the persona row gone nothing may still be telling the agent who it is — \
         an assembly that swaps in its own persona would then get two: {:?}",
        identity_claims(&without_persona)
    );
    assert!(
        without_persona.contains("describe_self"),
        "and the self-knowledge fragment is still there, so this is not passing by absence"
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
    // The agent exists, so its header is already on disk — one line, no
    // events. That the file is there before anything was said is the point of
    // a header: identity first, work after.
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        before.lines().count(),
        1,
        "the header and nothing else yet:\n{before}"
    );
    assert!(before.contains("\"header\""), "{before}");

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
    let log = app.context().only_session().unwrap();

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
    let id = app.context().only_session().unwrap().id().to_string();

    let outcome = run_turn(&app, "what session is this?")
        .await
        .expect("a turn");
    assert_eq!(outcome.tool_calls, 1);

    let logged = app
        .context()
        .only_session()
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

// ---- how to work it, not just what it is --------------------------------

#[tokio::test]
async fn every_mounted_row_can_describe_its_own_knobs() {
    let dir = scratch("operations");
    let app = start(tree(&dir, &[])).await;
    let ops = ask(&app, "operations").await;

    // Each of these is written by a different row, which is the point: no
    // central document could stay right about all of them.
    for expected in [
        "MEMORY",
        "RECALL",
        "SESSIONS",
        "MODEL",
        "HOW THIS SYSTEM IS PUT TOGETHER",
    ] {
        assert!(ops.contains(expected), "missing {expected}:\n{ops}");
    }
    // Real paths, not prose about paths.
    assert!(ops.contains("memory.md"), "{ops}");
    assert!(
        ops.contains(&dir.to_string_lossy().to_string()),
        "the memory paths must be this tree's, not a template:\n{ops}"
    );
}

#[tokio::test]
async fn the_reported_context_window_is_the_one_the_provider_reports() {
    // A number in prose is a claim, not a measurement. The judge here is
    // external: the `llm` seam's own `context_window()`, which is what the
    // compaction trigger divides by. If the description were written from the
    // row's raw config instead, the two would agree when a window is stated —
    // so the interesting half of this test is the tree where it is not.
    let dir = scratch("ctx-window");
    let stated = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [ { text = "ok" } ], context_window = 262144 }
"#;
    let app = start(tree(&dir, &[stated])).await;
    let reported = app
        .context()
        .service::<LlmSvc>()
        .expect("an llm seam")
        .context_window();
    assert_eq!(reported, 262_144, "the row's number reaches the provider");

    let ops = ask(&app, "operations").await;
    assert!(
        ops.contains("262144"),
        "the description must carry the window the provider reports:\n{ops}"
    );
    assert!(
        ops.contains("stated on the `llm` row"),
        "and must say where the number came from:\n{ops}"
    );
}

#[tokio::test]
async fn an_unstated_window_is_reported_as_a_default_not_as_a_fact() {
    // 128000 is what the adapter falls back to, and it is also a plausible
    // real window for a real model. Presented identically, a reader would take
    // the fallback for a property of the model they just mounted.
    let dir = scratch("ctx-default");
    let app = start(tree(&dir, &[])).await;
    let reported = app
        .context()
        .service::<LlmSvc>()
        .expect("an llm seam")
        .context_window();

    let ops = ask(&app, "operations").await;
    let line = ops
        .lines()
        .find(|l| l.contains("Context window:"))
        .unwrap_or_else(|| panic!("no window line to judge:\n{ops}"));
    assert!(
        line.contains(&reported.to_string()),
        "the fallback must still be the provider's own number:\n{line}"
    );
    assert!(
        line.contains("default"),
        "and must read as a fallback rather than as a fact about the model:\n{line}"
    );
}

#[tokio::test]
async fn the_env_model_rows_window_comes_from_its_own_row() {
    // The row a `--env-model` launch mounts, which is the one a person is most
    // likely to be running — and the one where the number has two possible
    // sources, because `context_window` may or may not be there. Both halves
    // are judged against the seam rather than against the sentence.
    let dir = scratch("ctx-env-row");
    std::env::set_var("ATOMCODE_TEST_WINDOW_KEY", "not-a-real-key");
    let row = r#"
[[patch]]
id = "llm"
name = "llm-openai-compat"
config = { base_url = "https://gw.invalid/v1", model = "some-model", api_key_env = "ATOMCODE_TEST_WINDOW_KEY", context_window = 1_000_000 }
"#;
    let app = start(tree(&dir, &[row])).await;
    std::env::remove_var("ATOMCODE_TEST_WINDOW_KEY");

    let reported = app
        .context()
        .service::<LlmSvc>()
        .expect("an llm seam")
        .context_window();
    assert_eq!(reported, 1_000_000, "the row's window reaches the provider");

    let ops = ask(&app, "operations").await;
    assert!(
        ops.contains("Context window: 1000000 tokens (stated on the `llm` row)"),
        "the env-model row must report its own configured window:\n{ops}"
    );
}

#[tokio::test]
async fn a_row_that_is_not_mounted_describes_nothing() {
    // The reason descriptions live on rows instead of in a document: a document
    // would confidently explain a capability this tree does not have.
    let dir = scratch("ops-absent");
    let app = start(tree(
        &dir,
        &[
            "[[patch]]\nid = \"memory\"\ndisabled = true",
            "[[patch]]\nid = \"recall\"\ndisabled = true",
        ],
    ))
    .await;
    let ops = ask(&app, "operations").await;
    assert!(!ops.contains("MEMORY"), "{ops}");
    assert!(!ops.contains("RECALL"), "{ops}");
    // …while the rows that ARE mounted still describe themselves, so this is
    // about removal and not about the registry having failed to fill.
    assert!(ops.contains("SESSIONS"), "{ops}");
}

#[tokio::test]
async fn the_settings_answer_is_the_settings_catalog_itself() {
    let dir = scratch("settings");
    let app = start(tree(&dir, &[])).await;
    let said = ask(&app, "settings").await;

    // The judge is the catalog, not a list this test also maintains: add a
    // setting anywhere in the workspace and this stays true with no edit here.
    assert!(
        said.contains(&format!(
            "{} of them are safely editable",
            atomcode_config::settings::SETTINGS.len()
        )),
        "{said}"
    );
    for spec in atomcode_config::settings::SETTINGS {
        assert!(said.contains(spec.id), "setting {} is missing", spec.id);
    }
    // The two questions a person actually asks.
    assert!(said.contains("language"), "{said}");
    assert!(
        said.contains("中文"),
        "aliases carry, so a Chinese ask still lands"
    );
    assert!(
        said.contains("config.toml"),
        "and it says which file to edit"
    );
}

#[tokio::test]
async fn settings_says_what_it_deliberately_does_not_cover() {
    // The catalog excludes model/provider/credentials by design. An answer that
    // silently omitted them would read as "you cannot change your model".
    let dir = scratch("settings-gap");
    let app = start(tree(&dir, &[])).await;
    let said = ask(&app, "settings").await;
    assert!(said.contains("deliberately absent"), "{said}");
    assert!(
        ask(&app, "operations").await.contains("MODEL"),
        "and the aspect it points at must actually answer it"
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

/// A knob nobody can read is a knob nobody can set.
///
/// `tool-web`'s search backend was hardwired until the coding tree turned the
/// row on and found the person's `provider` setting had nowhere to land. The
/// judge here is the same one this file uses everywhere: the answer must come
/// from the RUNNING tree, so the same tree without the setting must not say it.
#[tokio::test]
async fn the_web_row_reports_the_search_backend_it_was_given() {
    let root = scratch("web-backend");
    let told = start(tree(
        &root,
        &["[[patch]]\nid = \"tool-web\"\ndisabled = false\nconfig = { provider = \"duckduckgo\" }\n"],
    ))
    .await;
    let said = ask(&told, "operations").await;
    assert!(
        said.contains("duckduckgo"),
        "the row was given a backend and cannot say which:\n{said}"
    );

    let root = scratch("web-backend-default");
    let untold = start(tree(
        &root,
        &["[[patch]]\nid = \"tool-web\"\ndisabled = false\n"],
    ))
    .await;
    let said = ask(&untold, "operations").await;
    assert!(
        !said.contains("duckduckgo"),
        "unset, it must not claim a backend it was never given — otherwise the \
         assertion above passes against a hard-coded string:\n{said}"
    );
}
