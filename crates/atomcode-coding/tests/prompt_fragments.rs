//! A prompt fragment leaves with the row that contributed it.
//!
//! `PromptRegistry` has no idea who put a fragment there: `contribute` keys by
//! id and `remove` takes an id, so the only thing that withdraws a row's text
//! when the row goes is the `ctx.effect` the row filed next to its contribute.
//! The door (`plugins::tools::contribute_prompt`) does both halves; a row that
//! writes the halves by hand can write one and stop, and nothing says so —
//! the text simply stays in front of the model after its row is gone.
//!
//! So the judge here is a live tree: mount, patch the row off, read the prompt.
//! The control mounts the same tree and patches off a row that uses the door,
//! which is what makes this a statement about the missing half rather than
//! about `App::patch`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::plugins;
use atomcode_harness::seams::SystemPromptSvc;
use atomcode_plexus::{App, ConfigTree, Layer};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("prompt-frag-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The product's own overlay, so the rows under test are the product's rows.
fn tree(root: &std::path::Path) -> ConfigTree {
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
    let overlay = atomcode_coding::on_harness::coding_overlay(
        root,
        &root.join("artifacts"),
        atomcode_coding::on_harness::Presence::Attended,
        "a-model",
    );
    ConfigTree::from_layers(vec![
        atomcode_coding::on_harness::base_layer(),
        Layer::from_toml(&overlay).unwrap(),
        Layer::from_toml(&scoped).unwrap(),
    ])
    .unwrap()
}

async fn mounted(dir: &std::path::Path) -> App {
    let mut catalog = plugins::catalog();
    for plugin in atomcode_coding::on_harness::plugins() {
        catalog.register(plugin);
    }
    let mut app = App::new(catalog, tree(dir));
    app.start().await.expect("must mount");
    app
}

fn prompt(app: &App) -> String {
    app.context().service::<SystemPromptSvc>().unwrap().render()
}

async fn off(app: &mut App, row: &str) {
    app.patch(&Layer::from_toml(&format!("[[patch]]\nid = \"{row}\"\ndisabled = true\n")).unwrap())
        .await
        .expect("a patch that disables one row");
}

/// The identity paragraph is the first thing the model reads. A row that is no
/// longer mounted must not still be telling it who it is.
#[tokio::test]
async fn the_personas_row_takes_its_identity_away_when_it_goes() {
    let dir = scratch("persona");
    let mut app = mounted(&dir).await;
    assert!(
        prompt(&app).contains("You are AtomCode"),
        "the product persona should be in the prompt to begin with"
    );

    off(&mut app, "persona-atomcode").await;

    let after = prompt(&app);
    assert!(
        !after.contains("You are AtomCode"),
        "the row is gone and its identity is still in front of the model:\n{after}"
    );
}

/// The catalog the rest of coding's skill steering points at ("the
/// `=== AVAILABLE SKILLS ===` catalog above") must not outlive the row that
/// listed it.
#[tokio::test]
async fn the_skill_catalog_row_takes_its_catalog_away_when_it_goes() {
    let dir = scratch("skills");
    std::fs::create_dir_all(dir.join(".atomcode/skills/probe")).expect("skill dir");
    std::fs::write(
        dir.join(".atomcode/skills/probe/SKILL.md"),
        "---\nname: probe\ndescription: a skill that exists only to be listed\n---\n\nbody\n",
    )
    .expect("skill file");

    let mut app = mounted(&dir).await;
    let before = prompt(&app);
    if !before.contains("a skill that exists only to be listed") {
        // No catalog to withdraw: the row says nothing when there are no
        // skills, and a test that passes because nothing was there is not a
        // test. Say so rather than pass quietly.
        panic!("the skill catalog is not in the prompt to begin with:\n{before}");
    }

    off(&mut app, "skill-catalog-inline").await;

    let after = prompt(&app);
    assert!(
        !after.contains("a skill that exists only to be listed"),
        "the row is gone and its catalog is still in the prompt:\n{after}"
    );
}

/// Control. Same tree, same patch, a row that goes through the door — so a
/// failure above is the missing half, not `App::patch`.
#[tokio::test]
async fn a_row_that_uses_the_door_takes_its_fragment_away() {
    let dir = scratch("control");
    let mut app = mounted(&dir).await;
    assert!(prompt(&app).contains("assembled at runtime from plugin rows"));

    off(&mut app, "self-knowledge").await;

    let after = prompt(&app);
    assert!(
        !after.contains("assembled at runtime from plugin rows"),
        "the door's second half should have withdrawn it:\n{after}"
    );
}

/// Two rows must not write one fragment id.
///
/// Overwriting by id used to be how this product replaced the harness's generic
/// advertisement and persona. It reads like a replacement and behaves like a
/// race: the text depends on mount order, and whichever row unloads first takes
/// the other's fragment with it — `remove` is by id and does not know who wrote
/// what. So the replacement is a decision in the row list instead, where
/// `--dump-config` shows it.
#[test]
fn the_rows_this_product_replaces_are_not_in_its_tree() {
    let dir = scratch("assembly");
    let tree = tree(&dir);
    let active: Vec<&str> = tree.active().map(|e| e.id.as_str()).collect();

    assert!(
        active.contains(&"persona-atomcode"),
        "the product persona should be mounted: {active:?}"
    );
    assert!(
        !active.contains(&"persona-coding"),
        "the generic persona row is still mounted, so two rows write one \
         identity and the last to unload wins: {active:?}"
    );

    assert!(
        active.contains(&"skill-catalog-inline"),
        "the catalog row should be mounted: {active:?}"
    );
    assert!(
        !active.contains(&"skills-advert"),
        "the generic advertisement row is still mounted alongside the catalog \
         that replaces it: {active:?}"
    );
}
