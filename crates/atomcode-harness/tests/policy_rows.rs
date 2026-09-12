//! Rows that used to be flags, strings, or hardcoded middleware: permission
//! rules, plan mode, session titles, the human-question seam, telemetry and a
//! cost ceiling.

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::seams::{
    Question, SessionTitleSvc, StopReason, ToolsSvc, UserQuestions, UserQuestionsSvc, ANSWER_ALLOW,
    ANSWER_ALWAYS, ANSWER_DENY,
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

/// A human who allows every call, once.
struct AlwaysYes;

#[async_trait]
impl UserQuestions for AlwaysYes {
    fn describe(&self) -> String {
        "scripted human (always yes)".into()
    }
    async fn ask(&self, _question: &Question) -> Option<String> {
        Some(ANSWER_ALLOW.into())
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

// ---- sensitive paths --------------------------------------------------------

fn read_script(path: &std::path::Path) -> String {
    script_one(
        "read_file",
        &format!(r#"{{ file_path = {:?} }}"#, path.to_string_lossy()),
    )
}

fn secret_at(dir: &std::path::Path, rel: &str) -> PathBuf {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "hunter2").unwrap();
    path
}

#[tokio::test]
async fn a_safe_tool_reading_a_key_is_refused_by_default() {
    let dir = scratch("sensitive-deny");
    let key = secret_at(&dir, ".ssh/id_rsa");
    let app = start(tree(&dir, &read_script(&key), &[])).await;
    run_turn(&app, "read it").await.unwrap();
    let text = transcript(&app);
    assert!(
        text.contains("sensitive path"),
        "refused with the reason: {text}"
    );
    assert!(
        !text.contains("hunter2"),
        "and the secret never reached the model: {text}"
    );
}

#[tokio::test]
async fn an_env_template_is_not_a_secret() {
    let dir = scratch("sensitive-template");
    let template = secret_at(&dir, ".env.example");
    let app = start(tree(&dir, &read_script(&template), &[])).await;
    run_turn(&app, "read it").await.unwrap();
    assert!(transcript(&app).contains("hunter2"));
}

#[tokio::test]
async fn yolo_lets_a_sensitive_read_through() {
    let dir = scratch("sensitive-yolo");
    let key = secret_at(&dir, ".aws/credentials");
    let app = start(tree(
        &dir,
        &read_script(&key),
        &["[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }"],
    ))
    .await;
    run_turn(&app, "read it").await.unwrap();
    assert!(transcript(&app).contains("hunter2"));
}

/// A human who counts what they were asked, and allows each one.
struct CountingYes(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl UserQuestions for CountingYes {
    fn describe(&self) -> String {
        "scripted human (counting)".into()
    }
    async fn ask(&self, question: &Question) -> Option<String> {
        self.0.lock().unwrap().push(question.prompt.clone());
        Some(ANSWER_ALLOW.into())
    }
}

struct CountingYesPlugin(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Plugin for CountingYesPlugin {
    fn name(&self) -> &'static str {
        "user-questions-counting"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["user-questions"]
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UserQuestionsSvc>(Arc::new(CountingYes(self.0.clone())))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[tokio::test]
async fn interactive_approval_is_asked_about_the_sensitive_read_and_only_that() {
    let dir = scratch("sensitive-ask");
    let key = secret_at(&dir, ".ssh/id_ed25519");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut registry = plugins::catalog();
    registry.register(Arc::new(CountingYesPlugin(asked.clone())));
    let swap = "[[patch]]\nid = \"user-questions-unattended\"\nname = \"user-questions-counting\"";
    let mut app = App::new(
        registry,
        tree(&dir, &read_script(&key), &[bundle::INTERACTIVE, swap]),
    );
    app.start().await.unwrap();
    run_turn(&app, "read it").await.unwrap();
    assert!(transcript(&app).contains("hunter2"), "yes means yes");
    let questions = asked.lock().unwrap().clone();
    assert_eq!(
        questions.len(),
        1,
        "asked once, by the gate, not again by approval: {questions:?}"
    );
    assert!(
        questions[0].contains("sensitive path"),
        "and the question says why: {}",
        questions[0]
    );
}

/// A human who allows the first call for good, and would refuse afterwards.
///
/// The refusal is the point: if the grant is not remembered, the second write
/// is asked about and denied, and the test says so in the transcript.
struct AlwaysThenNo(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl UserQuestions for AlwaysThenNo {
    fn describe(&self) -> String {
        "scripted human (always, then no)".into()
    }
    async fn ask(&self, question: &Question) -> Option<String> {
        let mut asked = self.0.lock().unwrap();
        asked.push(question.prompt.clone());
        if asked.len() == 1 {
            Some(ANSWER_ALWAYS.into())
        } else {
            Some(ANSWER_DENY.into())
        }
    }
}

struct AlwaysThenNoPlugin(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Plugin for AlwaysThenNoPlugin {
    fn name(&self) -> &'static str {
        "user-questions-always-then-no"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["user-questions"]
    }
    fn description(&self) -> &'static str {
        "scripted human: allow_always once, then deny"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UserQuestionsSvc>(Arc::new(AlwaysThenNo(self.0.clone())))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[tokio::test]
async fn always_allow_is_asked_once_and_remembered_for_the_scope() {
    let dir = scratch("always");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut registry = plugins::catalog();
    registry.register(Arc::new(AlwaysThenNoPlugin(asked.clone())));
    let swap =
        "[[patch]]\nid = \"user-questions-unattended\"\nname = \"user-questions-always-then-no\"";
    // Two writes in one turn. `write_file` reports a tool-wide grant scope, so
    // an "always" on the first covers the second.
    let script = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [
  { text = "one", calls = [ { name = "write_file", args = { file_path = "a.txt", content = "a" } } ] },
  { text = "two", calls = [ { name = "write_file", args = { file_path = "b.txt", content = "b" } } ] },
  { text = "done" },
] }
"#;
    let mut app = App::new(registry, tree(&dir, script, &[bundle::INTERACTIVE, swap]));
    app.start().await.unwrap();
    run_turn(&app, "write both").await.unwrap();

    let questions = asked.lock().unwrap().clone();
    assert_eq!(
        questions.len(),
        1,
        "asked once; the second write is covered by the grant: {questions:?}"
    );
    assert!(
        dir.join("a.txt").exists() && dir.join("b.txt").exists(),
        "both writes ran"
    );
}

#[tokio::test]
async fn an_answer_nobody_offered_is_a_refusal() {
    struct Yes;
    #[async_trait]
    impl UserQuestions for Yes {
        fn describe(&self) -> String {
            "a human answering a question that was never asked".into()
        }
        async fn ask(&self, _question: &Question) -> Option<String> {
            // What the old prompt took as consent. It is not one of the
            // answers any more, and consent is only ever the words offered
            // for it.
            Some("yes".into())
        }
    }
    struct YesPlugin;
    #[async_trait]
    impl Plugin for YesPlugin {
        fn name(&self) -> &'static str {
            "user-questions-stale-yes"
        }
        fn provides(&self) -> &'static [&'static str] {
            &["user-questions"]
        }
        fn description(&self) -> &'static str {
            "scripted human answering with a word nobody offered"
        }
        async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
            let _ = ctx
                .provide::<UserQuestionsSvc>(Arc::new(Yes))
                .map_err(|e| e.to_string())?;
            Ok(())
        }
    }

    let dir = scratch("stale-yes");
    let mut registry = plugins::catalog();
    registry.register(Arc::new(YesPlugin));
    let swap = "[[patch]]\nid = \"user-questions-unattended\"\nname = \"user-questions-stale-yes\"";
    let script = script_one("write_file", r#"{ file_path = "a.txt", content = "a" }"#);
    let mut app = App::new(registry, tree(&dir, &script, &[bundle::INTERACTIVE, swap]));
    app.start().await.unwrap();
    run_turn(&app, "write it").await.unwrap();
    assert!(!dir.join("a.txt").exists(), "unrecognised is not consent");
    assert!(transcript(&app).contains("the user declined"));
}
