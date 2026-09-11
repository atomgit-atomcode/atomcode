//! The capability rows: each mounts real AtomCode capabilities, each is
//! optional, and each takes its guidance with it when it leaves.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::seams::{SessionSvc, SkillsSvc, SystemPromptSvc, ToolsSvc};
use atomcode_harness::session::{InjectionOrigin, SessionEvent};
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_plexus::{App, ConfigTree, Layer};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-caps-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn tree(root: &std::path::Path, script: &str, extra: &[&str]) -> ConfigTree {
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    let fs_row = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {:?} }}\n",
        root.to_string_lossy()
    );
    let loop_row = format!(
        "[[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 6, working_dir = {:?} }}\n",
        root.to_string_lossy()
    );
    // Keep the skill and memory rows pointed at the scratch tree, so a
    // developer's real skills and memory.md never leak into an assertion.
    let empty_home = root.join("__no_user_skills__");
    std::fs::create_dir_all(&empty_home).unwrap();
    let scoped = format!(
        "[[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut layers = vec![bundle::base().unwrap()];
    for src in [
        script,
        quiet,
        fs_row.as_str(),
        loop_row.as_str(),
        scoped.as_str(),
    ] {
        layers.push(Layer::from_toml(src).unwrap());
    }
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

const STOP: &str = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [ { text = "ok" } ] }
"#;

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
        .service::<SessionSvc>()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_symbol_layer_reads_real_code() {
    let dir = scratch("symbols");
    std::fs::write(
        dir.join("lib.rs"),
        "pub fn alpha() {}\npub struct Beta;\npub fn gamma(x: u32) -> u32 { x }\n",
    )
    .unwrap();
    let app = start(tree(
        &dir,
        &script_one("list_symbols", r#"{ file_path = "lib.rs" }"#),
        &[],
    ))
    .await;
    run_turn(&app, "what is in lib.rs").await.unwrap();
    let text = transcript(&app);
    for symbol in ["alpha", "Beta", "gamma"] {
        assert!(text.contains(symbol), "missing {symbol}: {text}");
    }
}

#[tokio::test]
async fn the_graph_layer_is_opt_in_and_brings_its_own_service() {
    let dir = scratch("graph");
    let base = start(tree(&dir, STOP, &[])).await;
    let names = base.context().service::<ToolsSvc>().unwrap().names();
    assert!(!names.contains(&"trace_callers".to_string()), "{names:?}");
    assert!(
        !base.context().service_names().contains(&"code-index"),
        "the shared index should not exist until the graph row is enabled"
    );
    drop(base);

    let full = start(tree(&dir, STOP, &[bundle::FULL])).await;
    let names = full.context().service::<ToolsSvc>().unwrap().names();
    for tool in [
        "trace_callers",
        "trace_callees",
        "blast_radius",
        "file_dependencies",
    ] {
        assert!(
            names.contains(&tool.to_string()),
            "missing {tool}: {names:?}"
        );
    }
    assert!(full.context().service_names().contains(&"code-index"));
}

#[tokio::test]
async fn skills_are_discovered_and_the_prompt_mentions_them_only_when_they_exist() {
    let dir = scratch("skills");
    // An empty tree: no project skills, so no catalog line.
    let bare = start(tree(&dir, STOP, &[])).await;
    let bare_prompt = bare
        .context()
        .service::<SystemPromptSvc>()
        .unwrap()
        .render();
    let bare_count = bare.context().service::<SkillsSvc>().unwrap().len();
    drop(bare);

    // Now give the project one.
    // `.claude/skills` rather than `.atomcode/skills`: the latter is rebased
    // onto ATOMCODE_HOME by `runtime_skill_dirs`, which the test isolates.
    let skill_dir = dir.join(".claude/skills/greet");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: greet\ndescription: say hello properly\n---\n\nSay hello.\n",
    )
    .unwrap();

    let app = start(tree(&dir, STOP, &[])).await;
    let ctx = app.context();
    assert_eq!(
        ctx.service::<SkillsSvc>().unwrap().len(),
        bare_count + 1,
        "the registry should pick up the project skill"
    );
    assert!(ctx
        .service::<ToolsSvc>()
        .unwrap()
        .names()
        .contains(&"use_skill".to_string()));
    let prompt = ctx.service::<SystemPromptSvc>().unwrap().render();
    assert!(prompt.contains("skill(s) are available"), "{prompt}");
    if bare_count == 0 {
        assert!(
            !bare_prompt.contains("skill(s) are available"),
            "an empty catalog must not advertise itself"
        );
    }
}

#[tokio::test]
async fn memory_is_injected_as_a_logged_fact_with_provenance() {
    let dir = scratch("memory");
    let project_memory = dir.join(".atomcode");
    std::fs::create_dir_all(&project_memory).unwrap();
    std::fs::write(
        project_memory.join("memory.md"),
        "- the user prefers concise answers\n",
    )
    .unwrap();

    let app = start(tree(&dir, STOP, &[])).await;
    run_turn(&app, "hello").await.unwrap();

    let log = app.context().service::<SessionSvc>().unwrap();
    let injected: Vec<_> = log
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Injected { text, origin, .. } => Some((text, origin)),
            _ => None,
        })
        .collect();
    assert_eq!(injected.len(), 1, "one injection, on the first turn");
    assert_eq!(injected[0].1, InjectionOrigin::Memory);
    assert!(injected[0].0.contains("concise answers"));

    // And it reaches the model through the projection, not around it.
    assert!(
        transcript(&app).contains("concise answers"),
        "memory must arrive the same way every other model-visible fact does"
    );
}

#[tokio::test]
async fn memory_is_injected_once_not_every_turn() {
    let dir = scratch("memory-once");
    let project_memory = dir.join(".atomcode");
    std::fs::create_dir_all(&project_memory).unwrap();
    std::fs::write(project_memory.join("memory.md"), "- remember this\n").unwrap();

    let app = start(tree(&dir, STOP, &[])).await;
    run_turn(&app, "first").await.unwrap();
    run_turn(&app, "second").await.unwrap();
    run_turn(&app, "third").await.unwrap();

    let count = app
        .context()
        .service::<SessionSvc>()
        .unwrap()
        .events()
        .into_iter()
        .filter(|e| matches!(e.event, SessionEvent::Injected { .. }))
        .count();
    assert_eq!(
        count, 1,
        "re-injecting every turn would break the cacheable prefix for no gain"
    );
}

#[tokio::test]
async fn removing_the_memory_row_removes_the_injection() {
    let dir = scratch("no-memory");
    let project_memory = dir.join(".atomcode");
    std::fs::create_dir_all(&project_memory).unwrap();
    std::fs::write(project_memory.join("memory.md"), "- should not appear\n").unwrap();

    let app = start(tree(&dir, STOP, &["[[remove]]\nid = \"memory\""])).await;
    run_turn(&app, "hello").await.unwrap();
    assert!(!transcript(&app).contains("should not appear"));
}

#[tokio::test]
async fn web_access_is_off_unless_the_config_asks_for_it() {
    let dir = scratch("web");
    let default = start(tree(&dir, STOP, &[])).await;
    let names = default.context().service::<ToolsSvc>().unwrap().names();
    assert!(!names.contains(&"web_search".to_string()), "{names:?}");
    drop(default);

    let full = start(tree(&dir, STOP, &[bundle::FULL])).await;
    let names = full.context().service::<ToolsSvc>().unwrap().names();
    assert!(names.contains(&"web_search".to_string()), "{names:?}");
    assert!(names.contains(&"web_fetch".to_string()), "{names:?}");
}

#[tokio::test]
async fn every_capability_row_can_be_removed_and_the_agent_still_runs() {
    let dir = scratch("minimal");
    let strip = r#"
[[remove]]
id = "skills"
[[remove]]
id = "codeintel"
[[remove]]
id = "memory"
[[remove]]
id = "tool-search-world"
[[remove]]
id = "tool-todo"
[[remove]]
id = "tool-bash-world"
[[remove]]
id = "persona-coding"
[[remove]]
id = "self-knowledge"
[[remove]]
id = "recall"
[[remove]]
id = "session-persistence-jsonl"
[[remove]]
id = "session-projection"
[[remove]]
id = "tool-result-cap"
[[remove]]
id = "tool-args-repair"
[[remove]]
id = "approval"
"#;
    let app = start(tree(&dir, STOP, &[strip])).await;
    let outcome = run_turn(&app, "still there?").await.unwrap();
    assert_eq!(outcome.text, "ok");
    let names = app.context().service::<ToolsSvc>().unwrap().names();
    assert_eq!(
        names,
        vec![
            "edit_file".to_string(),
            "list_directory".to_string(),
            "read_file".to_string(),
            "write_file".to_string()
        ],
        "what is left is exactly what the surviving rows mounted"
    );
}
