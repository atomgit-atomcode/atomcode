//! Rows that used to be flags, strings, or hardcoded middleware: permission
//! rules, plan mode, session titles, the human-question seam, telemetry and a
//! cost ceiling.

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::seams::{
    SessionTitleSvc, StopReason, ToolsSvc, UserQuestions, UserQuestionsSvc,
};
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
    let dir = std::env::temp_dir().join(format!("plexus-rows-{}-{tag}-{n}", std::process::id()));
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
         [[patch]]\nid = \"permissions\"\nconfig = {{ allow = [], deny = [], root = {root:?} }}\n",
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

fn script_one(tool: &str, args: &str) -> String {
    format!(
        r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = {{ script = [
  {{ text = "working", calls = [ {{ name = "{tool}", args = {args} }} ] }},
  {{ text = "done" }},
] }}
"#
    )
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

fn transcript(app: &App) -> String {
    app.context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_script(target: &std::path::Path) -> String {
    script_one(
        "write_file",
        &format!(
            r#"{{ file_path = {:?}, content = "written" }}"#,
            target.to_string_lossy()
        ),
    )
}

// ---- permission rules ---------------------------------------------------

#[tokio::test]
async fn an_allow_rule_settles_a_call_the_approval_row_would_have_refused() {
    let dir = scratch("allow");
    let target = dir.join("out.txt");
    let script = write_script(&target);

    // Baseline: deny-risky refuses the write.
    let app = start(tree(&dir, &script, &[])).await;
    run_turn(&app, "write").await.unwrap();
    assert!(!target.exists(), "the shipped default refuses it");
    drop(app);

    // One allow rule, and the approval row never gets asked.
    let allow = format!(
        "[[patch]]\nid = \"permissions\"\nconfig = {{ allow = [\"write_file\"], deny = [], root = {:?} }}\n",
        dir.to_string_lossy()
    );
    let app = start(tree(&dir, &script, &[allow.as_str()])).await;
    run_turn(&app, "write").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "written",
        "an allow rule pre-authorizes the call for every gate downstream"
    );
}

#[tokio::test]
async fn a_deny_rule_beats_an_otherwise_permissive_tree() {
    let dir = scratch("deny");
    std::fs::write(dir.join("a.txt"), "content").unwrap();
    let deny = format!(
        "[[patch]]\nid = \"permissions\"\nconfig = {{ allow = [], deny = [\"read_file\"], root = {:?} }}\n\n\
         [[patch]]\nid = \"approval\"\nconfig = {{ mode = \"yolo\" }}",
        dir.to_string_lossy()
    );
    let app = start(tree(
        &dir,
        &script_one("read_file", r#"{ file_path = "a.txt" }"#),
        &[deny.as_str()],
    ))
    .await;
    run_turn(&app, "read").await.unwrap();
    let text = transcript(&app);
    assert!(
        text.contains("denied by a `[permissions] deny` rule"),
        "{text}"
    );
    assert!(!text.contains("content"), "the read must not happen");
}

#[tokio::test]
async fn an_unparseable_rule_fails_the_row_instead_of_leaving_a_gap() {
    let dir = scratch("bad-rule");
    let bad =
        "[[patch]]\nid = \"permissions\"\nconfig = { allow = [\"Bash(unclosed\"], deny = [] }";
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, &script_one("read_file", "{}"), &[bad]),
    );
    let err = app.start().await.unwrap_err().to_string();
    assert!(err.contains("unparseable permission rules"), "{err}");
    assert!(err.contains("Bash(unclosed"), "{err}");
}

// ---- plan mode ----------------------------------------------------------

#[tokio::test]
async fn plan_mode_allows_reads_and_refuses_everything_else() {
    let dir = scratch("plan");
    std::fs::write(dir.join("a.txt"), "readable").unwrap();

    let reading = start(tree(
        &dir,
        &script_one("read_file", r#"{ file_path = "a.txt" }"#),
        &[bundle::PLAN],
    ))
    .await;
    run_turn(&reading, "look").await.unwrap();
    assert!(transcript(&reading).contains("readable"));
    let prompt = reading
        .context()
        .service::<atomcode_harness::seams::SystemPromptSvc>()
        .unwrap()
        .render();
    assert!(prompt.starts_with("PLAN MODE IS ACTIVE"), "{prompt}");
    drop(reading);

    // Even with approval wide open: plan mode is not an approval policy.
    let target = dir.join("out.txt");
    let yolo = "[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }";
    let writing = start(tree(&dir, &write_script(&target), &[bundle::PLAN, yolo])).await;
    run_turn(&writing, "write").await.unwrap();
    assert!(!target.exists());
    assert!(transcript(&writing).contains("plan mode is active"));
}

// ---- session title ------------------------------------------------------

#[tokio::test]
async fn a_session_is_named_from_its_first_prompt_without_a_model_call() {
    let dir = scratch("title");
    let app = start(tree(
        &dir,
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = { script = [ { text = \"ok\" } ] }",
        &[],
    ))
    .await;
    run_turn(&app, "make the build stop failing on windows please")
        .await
        .unwrap();
    run_turn(&app, "and then update the changelog")
        .await
        .unwrap();

    let ctx = app.context();
    let title = ctx
        .service::<SessionTitleSvc>()
        .unwrap()
        .title(&ctx.only_session().unwrap())
        .await
        .unwrap();
    assert_eq!(title, "make the build stop failing on windows please");
    drop(app);

    let shorter = "[[patch]]\nid = \"session-title-first-prompt\"\nconfig = { max_words = 3, max_bytes = 80 }";
    let app = start(tree(
        &dir,
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = { script = [ { text = \"ok\" } ] }",
        &[shorter],
    ))
    .await;
    run_turn(&app, "make the build stop failing").await.unwrap();
    let ctx = app.context();
    let title = ctx
        .service::<SessionTitleSvc>()
        .unwrap()
        .title(&ctx.only_session().unwrap())
        .await
        .unwrap();
    assert_eq!(title, "make the build");
}

// ---- user questions -----------------------------------------------------

/// A human who always says yes.
struct AlwaysYes;

#[async_trait]
impl UserQuestions for AlwaysYes {
    fn describe(&self) -> String {
        "scripted human (always yes)".into()
    }
    async fn ask(&self, _question: &str, _options: &[String]) -> Option<String> {
        Some("yes".into())
    }
}

struct AlwaysYesPlugin;

#[async_trait]
impl Plugin for AlwaysYesPlugin {
    fn name(&self) -> &'static str {
        "user-questions-always-yes"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["user-questions"]
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UserQuestionsSvc>(Arc::new(AlwaysYes))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[tokio::test]
async fn with_nobody_to_ask_an_interactive_approval_refuses() {
    let dir = scratch("unattended");
    let target = dir.join("out.txt");
    let app = start(tree(&dir, &write_script(&target), &[bundle::INTERACTIVE])).await;
    run_turn(&app, "write").await.unwrap();
    assert!(!target.exists(), "silence is not consent");
    assert!(transcript(&app).contains("the user declined"));
}

#[tokio::test]
async fn a_human_provider_turns_the_same_approval_row_interactive() {
    let dir = scratch("interactive");
    let target = dir.join("out.txt");
    let mut registry = plugins::catalog();
    registry.register(Arc::new(AlwaysYesPlugin));

    let swap =
        "[[patch]]\nid = \"user-questions-unattended\"\nname = \"user-questions-always-yes\"";
    let mut app = App::new(
        registry,
        tree(&dir, &write_script(&target), &[bundle::INTERACTIVE, swap]),
    );
    app.start().await.unwrap();
    run_turn(&app, "write").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "written",
        "the approval policy did not change — only who it asks"
    );
}

// ---- telemetry and budget ----------------------------------------------

#[tokio::test]
async fn telemetry_is_a_listener_that_writes_one_line_per_turn() {
    let dir = scratch("telemetry");
    let log = dir.join("metrics.jsonl");
    let row = format!(
        "[[patch]]\nid = \"telemetry\"\ndisabled = false\nconfig = {{ path = {:?} }}\n",
        log.to_string_lossy()
    );
    let app = start(tree(
        &dir,
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = { script = [ { text = \"ok\" } ] }",
        &[row.as_str()],
    ))
    .await;
    run_turn(&app, "one").await.unwrap();
    run_turn(&app, "two").await.unwrap();

    let text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["turn"], 1);
    assert_eq!(first["stop"], "Stopped");
    assert_eq!(
        first["tokens"]["prompt"], 100,
        "it reads the shared projection rather than counting again"
    );
}

#[tokio::test]
async fn a_token_budget_ends_a_turn_the_round_budget_would_have_allowed() {
    let dir = scratch("budget");
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    // The replay adapter reports 100 prompt tokens per round.
    let rows = "[[patch]]\nid = \"token-budget\"\nconfig = { max_prompt_tokens = 50 }\n\n\
                [[remove]]\nid = \"repeat-fuse\"\n\n\
                [[patch]]\nid = \"round-cap\"\nconfig = { max_rounds = 20 }";
    let script = format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  {}\n] }}\n",
        [r#"{ text = "again", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] }"#; 10].join(",\n  ")
    );
    let app = start(tree(&dir, &script, &[rows])).await;
    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(outcome.stop, StopReason::StoppedByPolicy);
    assert_eq!(outcome.rounds, 1, "the ceiling is checked every round");
}

#[tokio::test]
async fn the_todo_row_mounts_the_task_list() {
    let dir = scratch("todo");
    let app = start(tree(
        &dir,
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = { script = [ { text = \"ok\" } ] }",
        &[],
    ))
    .await;
    assert!(app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .names()
        .contains(&"todowrite".to_string()));
}
