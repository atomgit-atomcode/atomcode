//! Remembering across sessions.
//!
//! The interesting claims are not "the tool returns text". They are:
//!
//! * a session written *earlier* can be found by a session running *now*;
//! * another project's history is not in the results — which is the whole
//!   reason the store buckets by project, and the thing the old flat layout
//!   made structurally impossible;
//! * a turn this session compacted out of its own working set is still
//!   findable, because the log is not the working set;
//! * and a log written before bucketing existed still resumes.

use atomcode_harness::agent::OnlySession;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::seams::{SessionPersistenceSvc, ToolsSvc};
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_kernel::tool::{ProgressSink, ToolContext};
use atomcode_plexus::{App, ConfigTree, Layer};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-recall-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// `store` is shared across the apps a test starts, so "a later session finds an
/// earlier one" is testable at all; `project` is what the bucket is derived
/// from, so two projects can share one store and still not see each other.
fn tree(store: &Path, project: &Path, say: &str, extra: &[&str]) -> ConfigTree {
    let empty_home = store.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let scoped = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {project:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 8, working_dir = {project:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {project:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {project:?} }}\n\n\
         [[patch]]\nid = \"project-instructions\"\nconfig = {{ project_root = {project:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {store:?}, project_root = {project:?} }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [{{ text = \"{say}\" }}] }}\n",
        project = project.to_string_lossy(),
        home = empty_home.to_string_lossy(),
        store = store.to_string_lossy(),
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
    atomcode_harness::create_agent(&app).await.expect("an agent");
    app
}

async fn ask(app: &App, query: &str) -> String {
    let tool = app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .get("recall")
        .expect("recall must be registered");
    let ctx = ToolContext {
        working_dir: std::env::current_dir().unwrap(),
        cancel: Default::default(),
        progress: ProgressSink::noop(),
        requester: None,
    };
    let out = tool
        .execute(&serde_json::json!({ "query": query }).to_string(), &ctx)
        .await;
    out.content
}

/// Run one turn, then wait for the persistence listener to have written it.
async fn say_and_settle(app: &App, text: &str) -> String {
    run_turn(app, text).await.expect("a turn");
    let id = app
        .context()
        .only_session()
        .unwrap()
        .id()
        .to_string();
    let store = app.context().service::<SessionPersistenceSvc>().unwrap();
    for _ in 0..100 {
        if let Some(path) = store.location(&id) {
            // A missing file reads as empty, so the existence check the linter
            // objected to was buying nothing.
            if std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains(text)
            {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    id
}

// ---- across sessions -----------------------------------------------------

#[tokio::test]
async fn a_later_session_finds_what_an_earlier_one_said() {
    let store = scratch("store");
    let project = scratch("proj");

    let first = start(tree(&store, &project, "we chose argon2id", &[])).await;
    let old_id = say_and_settle(&first, "which password hash should we use?").await;
    drop(first);

    let second = start(tree(&store, &project, "ok", &[])).await;
    let found = ask(&second, "password hash").await;
    assert!(
        found.contains("argon2id"),
        "the earlier session's answer must come back:\n{found}"
    );
    assert!(
        found.contains(&old_id),
        "and it must say which session it came from, so the user can go look"
    );
}

#[tokio::test]
async fn another_projects_history_is_not_in_the_results() {
    // The reason the store buckets by project at all. Under the old flat layout
    // this assertion could not be made true by any amount of ranking.
    let store = scratch("shared-store");
    let mine = scratch("mine");
    let theirs = scratch("theirs");

    let other = start(tree(&store, &theirs, "the answer is quetzalcoatl", &[])).await;
    say_and_settle(&other, "what is the mascot called?").await;
    drop(other);

    let ours = start(tree(&store, &mine, "ok", &[])).await;
    let found = ask(&ours, "mascot").await;
    assert!(
        !found.contains("quetzalcoatl"),
        "another project's session must not surface here:\n{found}"
    );
    assert!(found.contains("No past turn"), "{found}");

    // …and it is genuinely there, so the assertion above is about scoping and
    // not about the write having silently failed.
    let theirs_again = start(tree(&store, &theirs, "ok", &[])).await;
    assert!(
        ask(&theirs_again, "mascot").await.contains("quetzalcoatl"),
        "the same query must hit in the project that said it"
    );
}

#[tokio::test]
async fn it_searches_the_current_session_too() {
    // Compaction takes turns out of the model's working set but never out of
    // the log, so the most valuable thing recall finds is often something this
    // very session already said.
    let store = scratch("current");
    let project = scratch("current-proj");
    let app = start(tree(&store, &project, "the port is 8443", &[])).await;
    say_and_settle(&app, "which port does the gateway listen on?").await;

    let found = ask(&app, "gateway port").await;
    assert!(found.contains("8443"), "{found}");
}

#[tokio::test]
async fn chinese_without_spaces_still_matches() {
    let store = scratch("cjk");
    let project = scratch("cjk-proj");
    let app = start(tree(&store, &project, "用的是 argon2id,不是 bcrypt", &[])).await;
    say_and_settle(&app, "密码哈希最后定的是哪个方案?").await;

    let found = ask(&app, "密码哈希").await;
    assert!(found.contains("argon2id"), "{found}");
}

#[tokio::test]
async fn a_query_that_matches_nothing_says_so_instead_of_inventing() {
    let store = scratch("empty");
    let project = scratch("empty-proj");
    let app = start(tree(&store, &project, "ok", &[])).await;
    say_and_settle(&app, "hello").await;
    let found = ask(&app, "kubernetes ingress").await;
    assert!(found.contains("No past turn"), "{found}");
    assert!(
        found.contains("rather than inventing"),
        "and it says what to do"
    );
}

// ---- the layout change ---------------------------------------------------

#[tokio::test]
async fn sessions_land_in_a_project_bucket_not_the_root() {
    let store = scratch("bucketed");
    let project = scratch("bucketed-proj");
    let app = start(tree(&store, &project, "ok", &[])).await;
    let id = say_and_settle(&app, "anything").await;

    let flat = store.join(format!("{id}.jsonl"));
    assert!(
        !flat.exists(),
        "not at the root any more: {}",
        flat.display()
    );

    let bucket = atomcode_config::util::stable_project_hash(&project);
    assert!(
        store.join(&bucket).join(format!("{id}.jsonl")).exists(),
        "in this project's bucket"
    );
}

#[tokio::test]
async fn a_log_written_before_bucketing_still_resumes() {
    // The migration that was deliberately not done. A hundred files on a user's
    // disk are not worth rewriting to tidy a directory, so the reader knows
    // both shapes and the old ones keep working where they lie.
    let store = scratch("legacy");
    let project = scratch("legacy-proj");
    std::fs::create_dir_all(&store).unwrap();
    let id = "1700000000000-999";
    std::fs::write(
        store.join(format!("{id}.jsonl")),
        "{\"seq\":1,\"event\":{\"kind\":\"turn_start\",\"turn\":1}}\n\
         {\"seq\":2,\"event\":{\"kind\":\"user_message\",\"turn\":1,\"text\":\"an old question\"}}\n",
    )
    .unwrap();

    let app = start(tree(
        &store,
        &project,
        "ok",
        &[&format!(
            "[[patch]]\nid = \"session\"\nconfig = {{ id = \"{id}\", resume = true }}\n\n\
             [[patch]]\nid = \"session-persistence-jsonl\"\n\
             config = {{ root = {store:?}, project_root = {project:?} }}",
            store = store.to_string_lossy(),
            project = project.to_string_lossy(),
        )],
    ))
    .await;

    let log = app.context().only_session().unwrap();
    assert_eq!(log.id(), id);
    assert!(
        log.len() >= 2,
        "the old file was replayed: {} events",
        log.len()
    );
    assert!(
        log.events()
            .iter()
            .any(|e| format!("{:?}", e.event).contains("an old question")),
        "and its content is there"
    );
}

// ---- negative control ----------------------------------------------------

#[tokio::test]
async fn without_the_row_there_is_no_way_to_search_the_log() {
    let store = scratch("no-recall");
    let project = scratch("no-recall-proj");
    let app = start(tree(
        &store,
        &project,
        "we chose argon2id",
        &["[[patch]]\nid = \"recall\"\ndisabled = true"],
    ))
    .await;
    say_and_settle(&app, "which password hash?").await;
    assert!(
        app.context()
            .service::<ToolsSvc>()
            .unwrap()
            .get("recall")
            .is_none(),
        "nothing else searches the log — that is the gap this row fills"
    );
}
