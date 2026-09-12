//! Subagents, and through them the two composability properties the realm
//! machinery exists for.
//!
//! Spatially: a child's tools, conversation and prompt are its own, and nothing
//! it installs reaches its parent or its siblings. Temporally: the child's whole
//! footprint is one fiber, so finishing reverts it.
//!
//! The property that matters most is the one that is *not* a check anywhere in
//! the subagent code: a policy the parent runs under still governs the child,
//! because realm visibility only runs one way.

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::events::{ToolExec, ToolsExecute};
use atomcode_harness::seams::{StopReason, SubagentsSvc, ToolsSvc};
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{App, ConfigTree, Layer, Next, Waterfall};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-sub-{}-{tag}-{n}", std::process::id()));
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
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 12, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"subagent-in-process\"\ndisabled = false\n",
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

/// The parent delegates once, then reports. The child's own script is the
/// remainder of the same replay list, because both agents share the `llm` slot.
fn delegating_script(task: &str, child_steps: &str) -> String {
    format!(
        r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = {{ script = [
  {{ text = "Delegating.", calls = [ {{ name = "task", args = {{ task = "{task}" }} }} ] }},
  {child_steps}
  {{ text = "The subagent reported back." }},
] }}
"#
    )
}

const YOLO: &str = "[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }";

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

/// The parent's conversation — empty when nothing ever spoke as the parent,
/// which is what a child spawned directly, with no turn around it, sees.
fn parent_transcript(app: &App) -> String {
    app.context()
        .only_session()
        .map(|log| {
            log.derive_messages()
                .iter()
                .map(|m| m.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_subagent_runs_in_its_own_realm_and_reports_back() {
    let dir = scratch("basic");
    std::fs::write(dir.join("a.txt"), "the answer is 42").unwrap();
    let script = delegating_script(
        "read a.txt",
        r#"{ text = "Looking.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
  { text = "It says the answer is 42." },"#,
    );
    let app = start(tree(&dir, &script, &[YOLO])).await;
    let outcome = run_turn(&app, "find out what a.txt says").await.unwrap();

    assert_eq!(outcome.stop, StopReason::Stopped);
    let text = parent_transcript(&app);
    assert!(
        text.contains("It says the answer is 42."),
        "the child's conclusion reaches the parent: {text}"
    );
    assert!(
        text.contains("[subagent:"),
        "with an accounting line: {text}"
    );
    assert!(
        !text.contains("the answer is 42\n"),
        "but the child's raw tool output stays in the child: {text}"
    );
}

#[tokio::test]
async fn the_child_gets_a_reduced_tool_catalog_and_the_parent_keeps_its_own() {
    let dir = scratch("catalog");
    // `write_file` is not in the subagent's allowed list, so the child is not
    // offered it and the call fails inside the child's realm.
    let script = delegating_script(
        "write something",
        r#"{ text = "Writing.", calls = [ { name = "write_file", args = { file_path = "out.txt", content = "x" } } ] },
  { text = "I could not write." },"#,
    );
    let app = start(tree(&dir, &script, &[YOLO])).await;
    run_turn(&app, "delegate a write").await.unwrap();

    assert!(
        !dir.join("out.txt").exists(),
        "the child had no write tool, so nothing was written"
    );
    let text = parent_transcript(&app);
    // The child's own error stays in the child — only its conclusion crosses.
    assert!(
        !text.contains("unknown tool"),
        "the parent should not be reading the child's tool errors: {text}"
    );
    assert!(text.contains("I could not write."), "{text}");

    // The parent's own catalog is untouched: it still has the tool.
    assert!(app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .names()
        .contains(&"write_file".to_string()));
}

/// Counts every tool call it sees, wherever it is dispatched from.
struct CountingGate {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Waterfall<ToolsExecute> for CountingGate {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        self.seen.lock().unwrap().push(exec.call.name.clone());
        next.run(exec).await
    }
}

/// Refuses everything, unconditionally.
struct DenyEverything;

#[async_trait]
impl Waterfall<ToolsExecute> for DenyEverything {
    async fn handle(&self, exec: &mut ToolExec, _next: Next<'_, ToolsExecute>) -> ToolResult {
        ToolResult {
            call_id: exec.call.id.clone(),
            content: "Refused by the root policy".into(),
            is_error: true,
            images: vec![],
        }
    }
}

#[tokio::test]
async fn a_root_policy_still_governs_the_child() {
    let dir = scratch("root-policy");
    std::fs::write(dir.join("a.txt"), "content").unwrap();
    let script = delegating_script(
        "read a.txt",
        r#"{ text = "Looking.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
  { text = "done" },"#,
    );
    let app = start(tree(&dir, &script, &[YOLO])).await;

    // Installed at the root, after mounting — exactly what a host-level gate
    // (credentials, audit, a kill switch) looks like.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let _guard = app
        .context()
        .on_waterfall::<ToolsExecute>(Arc::new(CountingGate { seen: seen.clone() }), true);

    run_turn(&app, "delegate").await.unwrap();

    let calls = seen.lock().unwrap().clone();
    assert!(
        calls.contains(&"task".to_string()),
        "the root gate sees the parent's call: {calls:?}"
    );
    assert!(
        calls.contains(&"read_file".to_string()),
        "and the child's, because a realm cannot escape upward: {calls:?}"
    );
}

#[tokio::test]
async fn a_root_denial_cannot_be_escaped_by_delegating() {
    let dir = scratch("root-deny");
    std::fs::write(dir.join("a.txt"), "secret").unwrap();
    let script = delegating_script(
        "read a.txt",
        r#"{ text = "Looking.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
  { text = "I could not read it." },"#,
    );
    let app = start(tree(&dir, &script, &[YOLO])).await;
    let _guard = app
        .context()
        .on_waterfall::<ToolsExecute>(Arc::new(DenyEverything), true);

    run_turn(&app, "delegate").await.unwrap();
    let text = parent_transcript(&app);
    assert!(
        !text.contains("secret"),
        "spawning a subagent must not be a way around a root refusal: {text}"
    );
}

#[tokio::test]
async fn the_childs_conversation_never_enters_the_parents_log() {
    let dir = scratch("log");
    std::fs::write(dir.join("a.txt"), "child-only detail").unwrap();
    let script = delegating_script(
        "read a.txt",
        r#"{ text = "Looking.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
  { text = "Summary only." },"#,
    );
    let app = start(tree(&dir, &script, &[YOLO])).await;
    run_turn(&app, "delegate").await.unwrap();

    let events = app.context().only_session().unwrap().events();
    let rendered = format!("{events:?}");
    assert!(
        !rendered.contains("child-only detail"),
        "the parent's log must not carry the child's intermediate reads"
    );
    assert!(
        parent_transcript(&app).contains("Summary only."),
        "only the child's conclusion crosses over"
    );
    // The accounting line reports how much the child did without carrying it.
    assert!(parent_transcript(&app).contains("round(s)"));
}

#[tokio::test]
async fn the_child_leaves_nothing_behind() {
    let dir = scratch("cleanup");
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let script = delegating_script(
        "read a.txt",
        r#"{ text = "Looking.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
  { text = "done" },"#,
    );
    let app = start(tree(&dir, &script, &[YOLO])).await;
    let before = app.context().service_names();
    let parent_tools = app.context().service::<ToolsSvc>().unwrap().names();

    run_turn(&app, "delegate").await.unwrap();

    assert_eq!(
        app.context().service_names(),
        before,
        "the child's realm services are gone with its fiber"
    );
    assert_eq!(
        app.context().service::<ToolsSvc>().unwrap().names(),
        parent_tools,
        "and the parent's catalog is exactly as it was"
    );
}

#[tokio::test]
async fn delegation_is_a_row_and_removing_it_removes_the_tool() {
    let dir = scratch("row");
    let app = start(tree(
        &dir,
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = { script = [ { text = \"ok\" } ] }",
        &["[[patch]]\nid = \"subagent-in-process\"\ndisabled = true"],
    ))
    .await;
    assert!(!app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .names()
        .contains(&"task".to_string()));
    assert!(!app.context().service_names().contains(&"subagents"));
}

#[tokio::test]
async fn the_allowed_tool_set_is_configuration() {
    let dir = scratch("allow");
    std::fs::write(dir.join("a.txt"), "readable").unwrap();
    // Spawned directly, so the replay script belongs to the child alone and the
    // outcome can be asserted exactly rather than inferred from a report.
    let script = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [
  { text = "Trying.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
  { text = "I have no tool for that." },
] }
"#;
    // Narrow the child to a set that excludes `read_file`.
    let narrow = "[[patch]]\nid = \"subagent-in-process\"\ndisabled = false\n\
                  config = { allowed_tools = [\"list_directory\"], max_rounds = 6 }";
    let app = start(tree(&dir, script, &[YOLO, narrow])).await;
    let describe = app.context().service::<SubagentsSvc>().unwrap().describe();
    assert!(describe.contains("list_directory"), "{describe}");
    assert!(!describe.contains("read_file"), "{describe}");

    let outcome = app
        .context()
        .service::<SubagentsSvc>()
        .unwrap()
        .spawn("read a.txt", "do the task")
        .await;
    assert_eq!(outcome.stop, StopReason::Stopped);
    assert_eq!(
        outcome.tool_calls, 1,
        "the child tried its one call and got nowhere"
    );
    assert_eq!(outcome.text, "I have no tool for that.");
    assert!(
        outcome.transcript_len > 0,
        "the child kept its own log, and the parent only hears the size"
    );
    assert!(
        !parent_transcript(&app).contains("readable"),
        "and the file it could not read never reached the parent either"
    );
}
