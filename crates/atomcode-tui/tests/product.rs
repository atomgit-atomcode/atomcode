//! The product, judged.
//!
//! `atui` is a coding agent with a screen in front of it, and the whole point of
//! `product.rs` is that this sentence is stated **once** — the row list lives in
//! `atomcode-coding`, and this crate stacks it rather than restating it. That
//! arrangement has one failure mode, and it is the one this file exists to
//! catch: `atomcode-coding` grows a row, nobody adds it here, and the product
//! ships a capability it names but does not mount. A person finds out one
//! support question at a time; `ast_grep`, the code graph, `open_file` and the
//! subagent were each found that way before.
//!
//! So the assertions are about *inclusion*, not about equality: every row
//! coding names must be addressable in this product's tree, and a row this
//! product adds (a panel, a surface) is none of coding's business.

use atomcode_coding::on_harness::{coding_overlay, Presence};
use atomcode_plexus::{App, Layer, Op};
use atomcode_tui::product;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("atui-product-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Every row id a layer addresses. Read out of the parsed layer rather than
/// grepped out of the TOML, so a row written with a different `id` is still
/// counted the way the loader counts it.
fn ids_named_by(src: &str) -> Vec<String> {
    let layer = Layer::from_toml(src).expect("layer parses");
    let mut out = Vec::new();
    for op in &layer.ops {
        match op {
            Op::Insert(entries) => out.extend(entries.iter().map(|e| e.id.clone())),
            Op::Patch { id, .. } => out.push(id.clone()),
            Op::Remove { id } => out.push(id.clone()),
        }
    }
    out
}

/// The two things `atui` overlays for a test: no tty, and no config.toml.
///
/// Both are the launcher's flags one line lower — `--offline` is a patch to
/// `llm`, `--headless` a patch to `surface` — so this is the product's own
/// assembly with the environment's two questions answered, not a second
/// assembly.
const SCRIPTED: &str = "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\n";

const NO_TTY: &str = "[[patch]]\nid = \"surface\"\nname = \"surface-headless\"\n";

fn assembled(home: &std::path::Path, extra: &[&str]) -> atomcode_plexus::ConfigTree {
    let assembly = product::assembly();
    let profiles = assembly.profiles().rooted_at(home).with_home();
    let mut overlays = vec![SCRIPTED, NO_TTY];
    overlays.extend_from_slice(extra);
    profiles
        .resolve(product::PROFILE, &overlays)
        .unwrap_or_else(|e| panic!("the product profile resolves: {e}"))
}

#[test]
fn every_row_coding_names_is_addressable_in_this_product() {
    // What the product claims to be, named by the crate that owns the claim.
    let working_dir = std::env::current_dir().expect("cwd");
    let artifacts = working_dir.join(".atomcode").join("artifacts");
    let coding = format!(
        "{}\n{}",
        atomcode_coding::on_harness::CODING_DEFAULTS,
        coding_overlay(&working_dir, &artifacts, Presence::Attended, "a-model-name"),
    );

    let tree = assembled(&scratch("gap"), &[]);
    let mounted: Vec<&str> = tree.entries.iter().map(|e| e.id.as_str()).collect();

    let missing: Vec<String> = ids_named_by(&coding)
        .into_iter()
        .filter(|id| !mounted.contains(&id.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "coding names {} row(s) this product never mounts: {missing:?}\n\
         A row list can only be changed by the crate that owns it; a host that \
         stacks it gets every row or the alignment is a story.",
        missing.len()
    );
}

#[test]
fn the_screen_drives_the_agent_and_the_generic_persona_stands_down() {
    let tree = assembled(&scratch("front"), &[]);
    let row = |id: &str| tree.entries.iter().find(|e| e.id == id);

    // The front end is this crate's, and coding's driver protocol is the row
    // that stands down — not the other way round, which would mean the screen
    // is drawn by nobody.
    assert_eq!(row("ui").expect("`ui` is mounted").name, "ui-tui2");
    assert!(
        row("ui-handle").is_some_and(|e| e.disabled),
        "`ui-handle` is disabled, not removed: it is still the way in for a \
         runtime that speaks the protocol"
    );
    // Two personas in one system prompt is worse than either, and coding's is
    // the one that says what this product is.
    assert!(
        row("persona-atomcode").is_some_and(|e| !e.disabled),
        "coding's persona is the one that runs"
    );
    assert!(
        row("persona-coding").is_none(),
        "the harness's generic persona must not be mounted beside it"
    );
}

#[test]
fn the_persons_own_patch_still_wins() {
    // The hazard this test exists for: a product layer that RESTATES a row
    // replaces it, so if the product's rows were stacked after the user's home
    // patch, this override would be silently reverted. It is asserted on a row
    // the product's own bundle is the one that turned ON.
    let home = scratch("homepatch");
    std::fs::write(
        home.join("harness.patch.toml"),
        "[[patch]]\nid = \"code-graph\"\ndisabled = true\n",
    )
    .expect("home patch");

    let tree = assembled(&home, &[]);
    let graph = tree
        .entries
        .iter()
        .find(|e| e.id == "code-graph")
        .expect("the product mounts `code-graph`");
    assert!(
        graph.disabled,
        "the person's own patch is the last word, including on the rows the \
         product turned on"
    );
}

#[tokio::test]
async fn the_product_mounts_and_audits_clean() {
    // Mounting is the only way to catch a row that names a plugin nobody
    // registered: the tree resolves happily, and the row is simply not there.
    let assembly = product::assembly();
    let mut app = App::new(assembly.catalog(), assembled(&scratch("mount"), &[]));
    app.start().await.unwrap_or_else(|e| panic!("mount: {e}"));

    let mut consumed: Vec<&str> = atomcode_harness::seam_map::HOST_CONSUMED.to_vec();
    consumed.extend_from_slice(&["tui-modules", "tui-commands"]);
    let findings = app.audit_with(&consumed, atomcode_harness::seam_map::HOST_PROVIDED);
    let defects: Vec<String> = findings
        .iter()
        .filter(|f| f.is_defect())
        .map(|f| f.to_string())
        .collect();
    assert!(defects.is_empty(), "composition defects: {defects:?}");

    // And the agent this screen drives is really there — a tree that audits
    // clean but hands over nothing is the failure mode `--audit` cannot see.
    assert!(
        app.context()
            .service::<atomcode_tui::plugin::AgentClientSvc>()
            .is_some(),
        "`ui-tui2` provides the command channel to its agent"
    );
}
