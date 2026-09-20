//! A product outside this workspace narrows the catalog of a coding tree.
//!
//! The mechanism is the harness's (`atomcode-harness/tests/tool_policy.rs`).
//! What this file pins is that it survives **this product's assembly**: coding
//! writes a long row list of its own, and a host's layer is the last one
//! applied, so the host's `tools` patch is not overwritten by anything coding
//! says about the same row. That is the whole of what a downstream product
//! needs in order to drop, replace or admit tools one at a time.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::plugins;
use atomcode_harness::seams::ToolsSvc;
use atomcode_plexus::{App, ConfigTree, Layer};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("coding-policy-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The product's own tree, plus whatever a host appended last.
fn tree(root: &std::path::Path, host: &str) -> ConfigTree {
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
    let mut layers = vec![
        atomcode_coding::on_harness::base_layer(),
        Layer::from_toml(&overlay).unwrap(),
        Layer::from_toml(&scoped).unwrap(),
    ];
    if !host.is_empty() {
        // Where `HostState::extra_layers` lands: after everything this product
        // wrote (`on_harness.rs`, `mount_swappable`).
        layers.push(Layer::from_toml(host).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

async fn mounted(dir: &std::path::Path, host: &str) -> App {
    let mut catalog = plugins::catalog();
    for plugin in atomcode_coding::on_harness::plugins() {
        catalog.register(plugin);
    }
    let mut app = App::new(catalog, tree(dir, host));
    app.start().await.expect("must mount");
    app
}

fn names(app: &App) -> Vec<String> {
    app.context().service::<ToolsSvc>().unwrap().names()
}

#[tokio::test]
async fn a_host_drops_one_tool_out_of_a_coding_tree() {
    let dir = scratch("drop");
    let before = names(&mounted(&dir, "").await);
    assert!(
        before.contains(&"write_file".to_string()),
        "the product mounts it by default: {before:?}"
    );

    let dir = scratch("drop-after");
    let after = names(
        &mounted(
            &dir,
            "[[patch]]\nid = \"tools\"\nconfig = { exclude = [\"write_file\"] }\n",
        )
        .await,
    );
    assert!(
        !after.contains(&"write_file".to_string()),
        "the host's layer is the last one applied and must win: {after:?}"
    );
    assert!(
        after.contains(&"read_file".to_string()) && after.contains(&"edit_file".to_string()),
        "and it drops one tool, not the row: {after:?}"
    );
}

/// The tools this product builds itself — the runtime's, not a row's — are in
/// the same catalog and go by the same policy.
#[tokio::test]
async fn a_host_can_drop_a_tool_this_product_added() {
    let dir = scratch("product-tool");
    let before = names(&mounted(&dir, "").await);
    assert!(
        before.contains(&"use_skill".to_string()),
        "coding mounts the skill tools: {before:?}"
    );

    let dir = scratch("product-tool-after");
    let after = names(
        &mounted(
            &dir,
            "[[patch]]\nid = \"tools\"\nconfig = { exclude = [\"use_skill\", \"list_skills\"] }\n",
        )
        .await,
    );
    assert!(
        !after
            .iter()
            .any(|n| n.ends_with("skill") || n.ends_with("skills")),
        "both should be gone: {after:?}"
    );
}
