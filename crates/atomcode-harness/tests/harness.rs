//! The architectural claims, as tests.
//!
//! Each one asserts something you could otherwise only assert in prose: that a
//! config row is what decides a behaviour, and that removing the row removes the
//! behaviour rather than falling back to a default hidden in the loop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::seams::{SessionSvc, StopReason, SystemPromptSvc, ToolsSvc};
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_plexus::{App, ConfigTree, Layer};

/// The base bundle mounts `session-persistence-jsonl`, which appends under
/// `ATOMCODE_HOME`. Redirect it before libtest spawns a thread.
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-harness-{}-{tag}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The base bundle with the model scripted and tracing silenced, plus whatever
/// layers a test wants on top.
fn tree(script: &str, working_dir: &Path, extra: &[&str]) -> ConfigTree {
    let quiet = r#"
[[patch]]
id = "trace"
config = { stream = false, tools = false, summary = false }
"#;
    let loop_row = format!(
        "[[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 8, working_dir = {:?} }}\n",
        working_dir.to_string_lossy()
    );
    // Point the execution world at the scratch dir. Without this the world's
    // containment root stays at the process cwd and every scratch path is
    // (correctly) refused — which is itself asserted in tests/world.rs.
    let fs_row = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {:?} }}\n",
        working_dir.to_string_lossy()
    );
    // Point the skill and memory rows at the scratch tree: a test must not read
    // the developer's real ~/.claude/skills or memory.md.
    let empty_home = working_dir.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let scoped = format!(
        "[[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n",
        root = working_dir.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut layers = vec![bundle::base().unwrap()];
    for src in [
        script,
        quiet,
        loop_row.as_str(),
        fs_row.as_str(),
        scoped.as_str(),
    ] {
        layers.push(Layer::from_toml(src).unwrap());
    }
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

/// Script one model turn that calls `tool` with `args`, then stops.
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
    app.start().await.expect("the base bundle must mount");
    app
}

/// The model's view of the conversation, projected from the log — the same
/// path the loop itself uses to build a request.
fn transcript(app: &App) -> String {
    app.context()
        .service::<SessionSvc>()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_base_bundle_mounts_and_fills_every_seam() {
    let dir = scratch("mount");
    let app = start(tree(&script_one("read_file", "{}"), &dir, &[])).await;
    let names = app.context().service_names();
    for seam in [
        "llm",
        "tools",
        "sessions",
        "session-projections",
        "session-persistence",
        "system-prompt",
        "agent-loop",
        "approval",
    ] {
        assert!(names.contains(&seam), "seam `{seam}` unfilled: {names:?}");
    }
}

#[tokio::test]
async fn a_turn_runs_the_real_tools_through_the_plugin_loop() {
    let dir = scratch("turn");
    std::fs::write(dir.join("hello.txt"), "from the harness").unwrap();
    let app = start(tree(
        &script_one("read_file", r#"{ file_path = "hello.txt" }"#),
        &dir,
        &[],
    ))
    .await;

    let outcome = run_turn(&app, "read hello.txt").await.unwrap();
    assert_eq!(outcome.stop, StopReason::Stopped);
    assert_eq!(outcome.tool_calls, 1);
    assert!(
        transcript(&app).contains("from the harness"),
        "the real read_file tool must have run"
    );
}

#[tokio::test]
async fn the_approval_row_decides_whether_a_write_lands() {
    let dir = scratch("approval");
    let target = dir.join("out.txt");
    let script = script_one(
        "write_file",
        &format!(
            r#"{{ file_path = {:?}, content = "written" }}"#,
            target.to_string_lossy()
        ),
    );

    // Shipped default: a risky call is refused, and the model is told why.
    let app = start(tree(&script, &dir, &[])).await;
    run_turn(&app, "write it").await.unwrap();
    assert!(!target.exists(), "deny-risky must not let the write land");
    assert!(transcript(&app).contains("Refused"));
    drop(app);

    // One row's config changes, nothing else.
    let yolo = "[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }";
    let app = start(tree(&script, &dir, &[yolo])).await;
    run_turn(&app, "write it").await.unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "written");
}

#[tokio::test]
async fn removing_the_approval_row_removes_the_gate_entirely() {
    let dir = scratch("no-approval");
    let target = dir.join("out.txt");
    let script = script_one(
        "write_file",
        &format!(
            r#"{{ file_path = {:?}, content = "ungated" }}"#,
            target.to_string_lossy()
        ),
    );
    let app = start(tree(&script, &dir, &["[[remove]]\nid = \"approval\""])).await;
    run_turn(&app, "write it").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "ungated",
        "with no approval row the loop has no gate to fall back on"
    );
    assert!(
        !app.context().service_names().contains(&"approval"),
        "and no approval service either"
    );
}

#[tokio::test]
async fn a_tool_row_owns_both_its_tools_and_its_prompt_guidance() {
    let dir = scratch("tool-row");
    let with_bash = start(tree(&script_one("read_file", "{}"), &dir, &[])).await;
    let ctx = with_bash.context();
    assert!(ctx
        .service::<ToolsSvc>()
        .unwrap()
        .names()
        .contains(&"bash".to_string()));
    assert!(ctx
        .service::<SystemPromptSvc>()
        .unwrap()
        .render()
        .contains("Shell commands run through"));
    drop(with_bash);

    let without = start(tree(
        &script_one("read_file", "{}"),
        &dir,
        &["[[remove]]\nid = \"tool-bash-world\""],
    ))
    .await;
    let ctx = without.context();
    assert!(
        !ctx.service::<ToolsSvc>()
            .unwrap()
            .names()
            .contains(&"bash".to_string()),
        "the model must not be offered a tool whose plugin is gone"
    );
    assert!(
        !ctx.service::<SystemPromptSvc>()
            .unwrap()
            .render()
            .contains("Shell commands run through"),
        "and guidance for it must go too"
    );
}

#[tokio::test]
async fn a_running_tree_can_swap_its_model_adapter() {
    let dir = scratch("hot-swap");
    let mut app = App::new(
        plugins::catalog(),
        tree(&script_one("read_file", "{}"), &dir, &[]),
    );
    app.start().await.unwrap();

    assert_eq!(run_turn(&app, "go").await.unwrap().tool_calls, 1);

    // Replace the `llm` row while the process runs. Nothing else is touched —
    // the loop, the tools and the policy rows keep their fibers.
    app.patch(
        &Layer::from_toml(
            r#"
            [[patch]]
            id = "llm"
            name = "llm-replay"
            config = { script = [ { text = "no tools this time" } ] }
            "#,
        )
        .unwrap(),
    )
    .await
    .unwrap();

    let after = run_turn(&app, "go").await.unwrap();
    assert_eq!(after.tool_calls, 0, "the new adapter is the one being used");
    assert_eq!(after.text, "no tools this time");
}

#[tokio::test]
async fn the_persona_is_a_fragment_not_a_privileged_message() {
    let dir = scratch("persona");
    let app = start(tree(&script_one("read_file", "{}"), &dir, &[])).await;
    let prompts = app.context().service::<SystemPromptSvc>().unwrap();
    assert!(prompts.render().contains("coding agent"));
    assert_eq!(
        prompts.ids(),
        vec![
            "persona-coding".to_string(),
            "self-knowledge".to_string(),
            "tool-fs-world".to_string(),
            "tool-bash-world".to_string(),
            "tool-search".to_string(),
            "tool-todo".to_string(),
            "codeintel".to_string(),
        ],
        "fragments are ranked, so the assembled prompt is stable across mount order"
    );
    drop(app);

    let bare = start(tree(
        &script_one("read_file", "{}"),
        &dir,
        &["[[remove]]\nid = \"persona-coding\""],
    ))
    .await;
    assert!(
        !bare
            .context()
            .service::<SystemPromptSvc>()
            .unwrap()
            .render()
            .contains("coding agent"),
        "removing the persona row leaves a harness with no coding opinion"
    );
}

#[tokio::test]
async fn an_unknown_tool_names_what_is_actually_mounted() {
    let dir = scratch("unknown");
    let app = start(tree(&script_one("no_such_tool", "{}"), &dir, &[])).await;
    run_turn(&app, "go").await.unwrap();
    let text = transcript(&app);
    assert!(text.contains("unknown tool `no_such_tool`"), "{text}");
    assert!(
        text.contains("read_file"),
        "the error lists the live catalog"
    );
}
