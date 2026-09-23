//! The capability rows: each mounts real AtomCode capabilities, each is
//! optional, and each takes its guidance with it when it leaves.

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::seams::{SkillsSvc, SystemPromptSvc, ToolsSvc};
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
        .only_session()
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

/// A capability row puts its own commands in the catalog, so a front end offers
/// them without knowing they exist
/// (`docs/adr/0021` §10, `docs/plans/2026-09-18-tui-panels-and-commands-inventory.md`
/// B1).
///
/// Four of them here: what skills are installed, and the three halves of
/// memory. Each is carried out by the capability the row already mounted — the
/// `/remember` command and the `memory` tool write the same file the same way,
/// because the command *is* the tool.
#[tokio::test]
async fn a_capability_row_offers_its_own_commands_and_they_do_the_work() {
    let dir = scratch("row-commands");
    let skill_dir = dir.join(".claude/skills/greet");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: greet\ndescription: say hello properly\n---\n\nSay hello.\n",
    )
    .unwrap();

    let app = start(tree(&dir, STOP, &[])).await;
    let ctx = app.context();
    let catalog = ctx
        .service::<atomcode_harness::seams::CommandsSvc>()
        .expect("the catalog is a core row");
    let agent = atomcode_harness::create_agent(&app)
        .await
        .expect("an agent to run commands against");

    let offered: Vec<String> = catalog
        .offered_for(&agent)
        .into_iter()
        .map(|c| c.name)
        .collect();
    for name in ["skills", "memory", "remember", "forget"] {
        assert!(
            offered.contains(&name.to_string()),
            "`{name}` is on offer: {offered:?}"
        );
    }

    // What is installed, by the row that loaded it.
    let listed = catalog
        .find("skills", &agent)
        .expect("offered, so findable")
        .run(agent.clone(), "")
        .await
        .expect("listing installed skills");
    assert!(
        listed.contains("greet") && listed.contains("say hello properly"),
        "the listing is what is installed, with what each is for: {listed}"
    );

    // And the write half does the work rather than describing it: remembering
    // something puts it where the next session reads it.
    catalog
        .find("remember", &agent)
        .expect("offered")
        .run(agent.clone(), "我偏好中文回复")
        .await
        .expect("remembering");
    let remembered = catalog
        .find("memory", &agent)
        .expect("offered")
        .run(agent.clone(), "")
        .await
        .expect("listing memory");
    assert!(
        remembered.contains("我偏好中文回复"),
        "what was remembered is there afterwards: {remembered}"
    );

    // An empty `remember` is refused rather than written: a memory of nothing
    // is a line every later session carries for no reason.
    assert!(catalog
        .find("remember", &agent)
        .expect("offered")
        .run(agent.clone(), "   ")
        .await
        .is_err());

    // A skill a person may invoke is a command too: `greet` is installed, so
    // `/greet` is in the menu — and running it queues the skill's prompt as
    // the person's own message, which is what makes it a turn they can answer,
    // undo and read back.
    assert!(
        offered.contains(&"greet".to_string()),
        "an installed skill is a command: {offered:?}"
    );
    let started = catalog
        .find("greet", &agent)
        .expect("offered")
        .run(agent.clone(), "世界")
        .await
        .expect("running a skill");
    assert!(
        started.contains("greet"),
        "it says what it started: {started}"
    );
    // Queued as the person's own message — waiting in the inbox for the turn
    // that will answer it. Not a hidden prepend and not the harness's voice:
    // `MessageOrigin::User`, so the turn it starts is theirs to undo.
    assert!(
        agent
            .inbox()
            .waiting_from(atomcode_harness::agent::MessageOrigin::User),
        "the skill's prompt is waiting as the person's own message"
    );

    // The reviewer is on offer too, from the row that mounts it. Inserted
    // explicitly, because that row is part of the coding assembly rather than
    // of this bundle — what is judged is that the row registers what it owns,
    // wherever it is mounted. Not run: it would spend a model round, and the
    // question here is the registration.
    drop(app);
    let reviewing = start(tree(
        &dir,
        STOP,
        &["[[insert]]\nname = \"tool-code-review\"\n"],
    ))
    .await;
    let review_agent = atomcode_harness::create_agent(&reviewing)
        .await
        .expect("an agent");
    let offered_now: Vec<String> = reviewing
        .context()
        .service::<atomcode_harness::seams::CommandsSvc>()
        .expect("the catalog")
        .offered_for(&review_agent)
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(
        offered_now.contains(&"review".to_string()),
        "the review row offers `/review`: {offered_now:?}"
    );
    drop(reviewing);
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

    let log = app.context().only_session().unwrap();
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
        .only_session()
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
id = "todo-reminder"
[[remove]]
id = "tool-ask"
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
            // Bound to the same world as the rest of `tool-fs-world`, which is
            // why it belongs in that row and not beside it.
            "search_replace".to_string(),
            "write_file".to_string()
        ],
        "what is left is exactly what the surviving rows mounted"
    );
}

// ---- the list (`todo-reminder`) --------------------------------------------
//
// Moved here from `atomcode-coding/tests/ask_and_todo.rs`: coding keeps this row
// off (its own `TodoHook` says the same thing, once, without logging it), so the
// row's criteria belong to the tree that mounts it.

fn replay(steps: &str) -> String {
    format!("[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {steps} ] }}\n")
}

/// Room for a long task: the tree's six rounds end these scripts early. A patch
/// replaces the row's whole config, so the working directory comes along.
fn rounds(root: &std::path::Path) -> String {
    format!(
        "[[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 20, working_dir = {:?} }}\n",
        root.to_string_lossy()
    )
}

/// What the harness told the model about the list without the person saying it.
fn task_list_notes(app: &App) -> Vec<String> {
    app.context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Injected {
                text,
                origin: InjectionOrigin::Reminder,
                ..
            } if text.contains("task list") => Some(text),
            _ => None,
        })
        .collect()
}

/// Plan two tasks, then work for three steps without touching the list.
///
/// `list_directory` is the filler on purpose: it is safe, it always succeeds,
/// and it is exactly the shape of the work that makes a model forget — read
/// something, read something else, read something else.
const FORGETS: &str = r#"{ text = "Planning.", calls = [ { name = "todowrite", args = { todos = [ { content = "read the parser", status = "in_progress" }, { content = "fix the parser", status = "pending" } ] } } ] },
   { text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
   { text = "Still looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
   { text = "And again.", calls = [ { name = "list_directory", args = { path = "." } } ] },
   { text = "Done." }"#;

#[tokio::test]
async fn a_list_that_stopped_describing_the_work_is_said_so_once() {
    let dir = scratch("stale");
    let after_two = "[[patch]]\nid = \"todo-reminder\"\nconfig = { after_steps = 2 }";
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, &replay(FORGETS), &[after_two, &rounds(&dir)]),
    );
    app.start().await.unwrap();
    run_turn(&app, "fix the parser").await.unwrap();

    let said = task_list_notes(&app);
    assert!(
        !said.is_empty(),
        "three steps of silence with a task in progress is stale"
    );
    assert!(
        said[0].contains("read the parser"),
        "it names the task the list still claims: {}",
        said[0]
    );
    assert!(
        said[0].contains("<system-reminder>") && said[0].contains("Do not mention"),
        "and it is machinery, not something to read aloud: {}",
        said[0]
    );
    // Spacing, not silence: a reminder every step is noise, and noise is what a
    // model learns to skip.
    assert!(
        said.len() <= 2,
        "one reminder per stretch of silence, not one per step: {said:?}"
    );
}

/// A task that takes many steps is told about its list once, not every few
/// steps — and moving the list starts a new stretch.
///
/// The row used to repeat itself every `after_steps` of silence. On one real
/// regression hunt that was seven notes ("not updated for 3 … 21 steps"), and
/// the model answered nearly each one by restating its whole diagnosis to show
/// it was still on the task.
#[tokio::test]
async fn a_long_task_is_reminded_once_per_stretch_of_silence() {
    let long = r#"{ text = "Planning.", calls = [ { name = "todowrite", args = { todos = [ { content = "read the parser", status = "in_progress" }, { content = "fix the parser", status = "pending" } ] } } ] },
       { text = "1", calls = [ { name = "glob", args = { pattern = "*.step1" } } ] },
       { text = "2", calls = [ { name = "glob", args = { pattern = "*.step2" } } ] },
       { text = "3", calls = [ { name = "glob", args = { pattern = "*.step3" } } ] },
       { text = "4", calls = [ { name = "glob", args = { pattern = "*.step4" } } ] },
       { text = "5", calls = [ { name = "glob", args = { pattern = "*.step5" } } ] },
       { text = "6", calls = [ { name = "glob", args = { pattern = "*.step6" } } ] },
       { text = "7", calls = [ { name = "glob", args = { pattern = "*.step7" } } ] },
       { text = "Moving on.", calls = [ { name = "todowrite", args = { action = "update", id = 1, status = "completed" } }, { name = "todowrite", args = { action = "update", id = 2, status = "in_progress" } } ] },
       { text = "8", calls = [ { name = "glob", args = { pattern = "*.step8" } } ] },
       { text = "9", calls = [ { name = "glob", args = { pattern = "*.step9" } } ] },
       { text = "10", calls = [ { name = "glob", args = { pattern = "*.step10" } } ] },
       { text = "11", calls = [ { name = "glob", args = { pattern = "*.step11" } } ] },
       { text = "Done." }"#;
    let dir = scratch("long");
    let after_two = "[[patch]]\nid = \"todo-reminder\"\nconfig = { after_steps = 2 }";
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, &replay(long), &[after_two, &rounds(&dir)]),
    );
    app.start().await.unwrap();
    let out = run_turn(&app, "fix the parser").await.unwrap();
    assert_eq!(out.text, "Done.", "the whole script ran");

    // Only this row's notes: other rows (the repeat fuse) inject reminders too.
    // The filler calls differ from each other so that the fuse stays out of it.
    let said: Vec<String> = task_list_notes(&app)
        .into_iter()
        .filter(|text| text.contains("task list"))
        .collect();
    assert_eq!(
        said.len(),
        2,
        "seven quiet steps, a move, four more: one note per stretch: {said:?}"
    );
    assert!(said[0].contains("read the parser"), "{}", said[0]);
    assert!(
        said[1].contains("fix the parser"),
        "the second stretch is about the item it moved to: {}",
        said[1]
    );
}

#[tokio::test]
async fn a_list_kept_up_to_date_is_never_mentioned() {
    // The same work, with the model doing what the tool asked of it. Nothing
    // about this turn is worth a sentence.
    let keeps_up = r#"{ text = "Planning.", calls = [ { name = "todowrite", args = { todos = [ { content = "read the parser", status = "in_progress" } ] } } ] },
       { text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
       { text = "Done that.", calls = [ { name = "todowrite", args = { action = "update", id = 1, status = "completed" } } ] },
       { text = "Finished." }"#;
    let dir = scratch("tidy");
    let after_two = "[[patch]]\nid = \"todo-reminder\"\nconfig = { after_steps = 2 }";
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, &replay(keeps_up), &[after_two, &rounds(&dir)]),
    );
    app.start().await.unwrap();
    run_turn(&app, "fix the parser").await.unwrap();

    assert!(
        task_list_notes(&app).is_empty(),
        "a list that is true says nothing: {:?}",
        task_list_notes(&app)
    );
}

#[tokio::test]
async fn a_session_that_never_planned_is_left_alone() {
    // The row must not become a nag about using `todowrite` at all. A one-line
    // fix does not need a plan, and the tool says so itself.
    let dir = scratch("no-plan");
    let never = r#"{ text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
       { text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
       { text = "Looking.", calls = [ { name = "list_directory", args = { path = "." } } ] },
       { text = "Done." }"#;
    let after_two = "[[patch]]\nid = \"todo-reminder\"\nconfig = { after_steps = 2 }";
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, &replay(never), &[after_two, &rounds(&dir)]),
    );
    app.start().await.unwrap();
    run_turn(&app, "look around").await.unwrap();
    assert!(
        task_list_notes(&app).is_empty(),
        "{:?}",
        task_list_notes(&app)
    );
}
