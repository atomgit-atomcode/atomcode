//! The architectural claims, as tests.
//!
//! Each one asserts something you could otherwise only assert in prose: that a
//! config row is what decides a behaviour, and that removing the row removes the
//! behaviour rather than falling back to a default hidden in the loop.

use atomcode_harness::agent::OnlySession;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::seams::{StopReason, SystemPromptSvc, ToolsSvc};
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
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"project-instructions\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n",
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
        .only_session()
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
        "session-projections",
        "session-persistence",
        "system-prompt",
        "agent-loop",
        "approval",
    ] {
        assert!(names.contains(&seam), "seam `{seam}` unfilled: {names:?}");
    }
    // `sessions` is filled per agent, not per tree: it exists once an agent does.
    assert!(!names.contains(&"sessions"), "no agent, no log: {names:?}");
    atomcode_harness::create_agent(&app).await.unwrap();
    assert!(app.context().only_session().is_some());
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
            "tool-search-world".to_string(),
            "tool-todo".to_string(),
            "tool-ask".to_string(),
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

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

/// Facts reach the file in the order they were committed.
///
/// A sequence number is minted when a fact commits, so the log's ORDER is the
/// conversation. Nothing sorts on the way back in: `JsonlStore::parse` reads
/// lines in file order, `SessionLog::restore` stores them as given, and
/// `derive_messages` folds them as stored. So a file written out of order is a
/// resumed conversation in the wrong order — two tool results swapped, or
/// context landing after the message it was meant to precede.
///
/// This was not a hypothetical: persistence used to `tokio::spawn` one task per
/// committed fact, and the tasks raced. Real sessions on disk came out with
/// 10–18% of all facts out of order, and 2–10 of the model-visible ones each.
/// It went unnoticed because every other test either disables persistence or
/// only checks that the events are PRESENT.
///
/// `multi_thread` is load-bearing. On the default current-thread runtime the
/// spawned writes drain in spawn order and this scenario passes against the
/// very bug it was written for — verified by putting the bug back. The engine
/// that wrote those real sessions is multi-threaded, so the test's runtime has
/// to be too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_log_reaches_the_disk_in_the_order_it_was_committed() {
    let dir = scratch("persist-order");
    let file = dir.join("note.txt");
    let app = start(tree(
        &script_one(
            "write_file",
            &format!(
                "{{ file_path = {:?}, content = \"x\" }}",
                file.to_string_lossy()
            ),
        ),
        &dir,
        &[],
    ))
    .await;
    run_turn(&app, "write it").await.unwrap();
    // A second turn, so there is enough traffic for a race to show.
    run_turn(&app, "and again").await.unwrap();

    let root = atomcode_harness::home().join("sessions");
    // Wait for the writer to drain rather than guessing how long it takes: the
    // queue is deliberately off the turn's path, so the last facts are still in
    // flight when the turn returns. A fixed sleep here was flaky under a full
    // parallel run — which is the one condition that matters, because that is
    // when the writer is slowest.
    for _ in 0..200 {
        let mut seen = Vec::new();
        walk(&root, &mut seen);
        if seen.iter().any(|p| {
            std::fs::read_to_string(p)
                .map(|t| t.lines().filter(|l| l.contains("turn_end")).count() >= 2)
                .unwrap_or(false)
        }) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let mut written: Vec<PathBuf> = Vec::new();
    walk(&root, &mut written);
    assert!(
        !written.is_empty(),
        "nothing was persisted under {root:?} — this scenario would pass on an \
         empty directory otherwise, which is the failure it exists to catch"
    );

    for path in written {
        let text = std::fs::read_to_string(&path).expect("read the session back");
        let mut seqs = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{}: not JSON: {e}", path.display()));
            // The header carries no event and no sequence number.
            if value.get("header").is_some() && value.get("event").is_none() {
                continue;
            }
            seqs.push(
                value
                    .get("seq")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            );
        }
        assert!(
            seqs.len() > 4,
            "{}: too few facts to mean anything",
            path.display()
        );
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(
            seqs,
            sorted,
            "{}: the file is not in sequence order — a resume would replay this \
             conversation scrambled",
            path.display()
        );
    }
}
