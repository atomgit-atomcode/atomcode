//! Profiles: named assemblies, the layer order, and the user's last word.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::profile::Profiles;
use atomcode_harness::seams::UiSvc;
use atomcode_harness::{bundle, plugins};
use atomcode_plexus::App;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// A private harness home, so profiles written by one test are invisible to the
/// others and to the developer's real one.
fn home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-home-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("profiles")).expect("home");
    dir
}

fn write(path: PathBuf, contents: &str) {
    std::fs::write(path, contents).expect("write");
}

const QUIET: &str =
    "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";

#[test]
fn the_shipped_profiles_all_resolve() {
    let profiles = Profiles::builtin();
    assert!(profiles.names().len() >= 8);
    for name in profiles.names() {
        let tree = profiles
            .resolve(name, &[])
            .unwrap_or_else(|e| panic!("`{name}`: {e}"));
        assert!(
            tree.entries.iter().any(|e| e.id == "ui"),
            "`{name}` assembles no front end"
        );
    }
}

#[test]
fn a_profile_is_bundles_plus_its_own_patch() {
    let profiles = Profiles::builtin();
    // `headless` is `base + oneshot-app` plus a patch that silences rendering
    // and drops persistence — none of which the bundles themselves know about.
    let tree = profiles.resolve("headless", &[]).unwrap();
    let trace = tree.entries.iter().find(|e| e.id == "trace").unwrap();
    assert_eq!(trace.config["stream"], false);
    let persistence = tree
        .entries
        .iter()
        .find(|e| e.id == "session-persistence-jsonl")
        .unwrap();
    assert!(persistence.disabled);

    // The same bundles without that patch leave both alone.
    let plain = profiles.resolve("oneshot", &[]).unwrap();
    let trace = plain.entries.iter().find(|e| e.id == "trace").unwrap();
    assert_eq!(trace.config["stream"], true);
}

#[test]
fn different_profiles_put_a_different_front_end_behind_the_same_agent() {
    let profiles = Profiles::builtin();
    let front_end = |name: &str| {
        profiles
            .resolve(name, &[])
            .unwrap()
            .entries
            .iter()
            .find(|e| e.id == "ui")
            .unwrap()
            .name
            .clone()
    };
    assert_eq!(front_end("oneshot"), "ui-oneshot");
    assert_eq!(front_end("repl"), "ui-repl");
    assert_eq!(front_end("web"), "ui-web");
    assert_eq!(front_end("sdk"), "ui-jsonrpc");
    assert_eq!(front_end("embed"), "ui-quiet");

    // And everything below is the same rows in each case — with one named
    // exception. `tool-open-file` acts on the machine the *person* is at, so it
    // rides only with a front end that has a person at a display; every tool
    // that acts on the agent's own world is there for all of them.
    let tools_in = |name: &str| {
        let mut ids: Vec<String> = profiles
            .resolve(name, &[])
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.id.starts_with("tool-") && !e.disabled)
            .map(|e| e.id.clone())
            .collect();
        ids.sort();
        ids
    };
    let world_tools = |name: &str| {
        let mut ids = tools_in(name);
        ids.retain(|id| id != "tool-open-file");
        ids
    };
    assert_eq!(world_tools("oneshot"), world_tools("web"));
    assert_eq!(world_tools("repl"), world_tools("headless"));
    {
        let with_person = "repl";
        assert!(
            tools_in(with_person).contains(&"tool-open-file".to_string()),
            "`{with_person}` has a person to show files to"
        );
    }
    for nobody in ["oneshot", "web", "sdk", "embed", "headless"] {
        assert!(
            !tools_in(nobody).contains(&"tool-open-file".to_string()),
            "`{nobody}` has nobody at this machine's display"
        );
    }
}

#[test]
fn the_home_patch_lands_after_the_profile() {
    let dir = home("home-patch");
    write(
        dir.join("harness.patch.toml"),
        "[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }",
    );
    let profiles = Profiles::builtin().rooted_at(&dir).with_home();
    let tree = profiles.resolve("oneshot", &[]).unwrap();
    let approval = tree.entries.iter().find(|e| e.id == "approval").unwrap();
    assert_eq!(
        approval.config["mode"], "yolo",
        "the person running it overrules what the profile chose"
    );
}

#[test]
fn an_overlay_gets_the_last_word_over_the_home_patch() {
    let dir = home("overlay-wins");
    write(
        dir.join("harness.patch.toml"),
        "[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }",
    );
    let profiles = Profiles::builtin().rooted_at(&dir).with_home();
    let tree = profiles
        .resolve(
            "oneshot",
            &["[[patch]]\nid = \"approval\"\nconfig = { mode = \"read-only\" }"],
        )
        .unwrap();
    let approval = tree.entries.iter().find(|e| e.id == "approval").unwrap();
    assert_eq!(approval.config["mode"], "read-only");
}

#[test]
fn a_deployment_can_add_a_profile_by_dropping_a_file() {
    let dir = home("custom");
    write(
        dir.join("profiles/audit-only.toml"),
        r#"
bundles = ["base", "sdk-app"]
description = "read-only JSON-RPC for an auditor"
patch = """
[[patch]]
id = "fs"
name = "fs-readonly"

[[patch]]
id = "approval"
config = { mode = "read-only" }
"""
"#,
    );
    let profiles = Profiles::builtin().rooted_at(&dir).with_home();
    assert!(profiles.names().contains(&"audit-only"));

    let tree = profiles.resolve("audit-only", &[]).unwrap();
    assert_eq!(
        tree.entries.iter().find(|e| e.id == "ui").unwrap().name,
        "ui-jsonrpc"
    );
    assert_eq!(
        tree.entries.iter().find(|e| e.id == "fs").unwrap().name,
        "fs-readonly"
    );
    assert_eq!(
        tree.entries
            .iter()
            .find(|e| e.id == "approval")
            .unwrap()
            .config["mode"],
        "read-only"
    );
}

#[tokio::test]
async fn a_profile_added_from_disk_actually_mounts() {
    let dir = home("custom-mounts");
    write(
        dir.join("profiles/mine.toml"),
        r#"
bundles = ["base", "embed-app"]
description = "mine"
"#,
    );
    let profiles = Profiles::builtin().rooted_at(&dir).with_home();
    let tree = profiles.resolve("mine", &[bundle::OFFLINE, QUIET]).unwrap();
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.unwrap();
    assert_eq!(
        app.context().service::<UiSvc>().unwrap().describe(),
        "no front end; the embedder drives"
    );
}

#[test]
fn a_file_can_redefine_a_shipped_profile() {
    let dir = home("redefine");
    write(
        dir.join("profiles/repl.toml"),
        r#"
bundles = ["base", "web-app"]
description = "here, repl means the browser"
"#,
    );
    let profiles = Profiles::builtin().rooted_at(&dir).with_home();
    let tree = profiles.resolve("repl", &[]).unwrap();
    assert_eq!(
        tree.entries.iter().find(|e| e.id == "ui").unwrap().name,
        "ui-web",
        "a deployment decides what its own profile names mean"
    );
}

#[test]
fn an_unknown_profile_names_the_ones_that_exist() {
    let err = Profiles::builtin()
        .resolve("nope", &[])
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown profile `nope`"), "{err}");
    assert!(err.contains("oneshot"), "{err}");
}

#[test]
fn a_profile_naming_a_missing_bundle_says_so() {
    let dir = home("bad-bundle");
    write(
        dir.join("profiles/broken.toml"),
        "bundles = [\"base\", \"no-such-bundle\"]\ndescription = \"x\"",
    );
    let profiles = Profiles::builtin().rooted_at(&dir).with_home();
    let err = profiles.resolve("broken", &[]).unwrap_err().to_string();
    assert!(err.contains("no-such-bundle"), "{err}");
    assert!(
        err.contains("base"),
        "the message lists what does exist: {err}"
    );
}

#[test]
fn a_malformed_profile_is_skipped_not_fatal() {
    let dir = home("malformed");
    write(dir.join("profiles/broken.toml"), "this is not toml [[[");
    write(
        dir.join("profiles/good.toml"),
        "bundles = [\"base\", \"embed-app\"]\ndescription = \"fine\"",
    );
    let profiles = Profiles::builtin().rooted_at(&dir).with_home();
    assert!(profiles.names().contains(&"good"));
    assert!(!profiles.names().contains(&"broken"));
    assert!(
        profiles.resolve("oneshot", &[]).is_ok(),
        "the rest still work"
    );
}

#[test]
fn explain_states_the_layer_order() {
    let text = Profiles::builtin().explain("headless");
    assert!(text.contains("bundle `base`"));
    assert!(text.contains("bundle `oneshot-app`"));
    assert!(text.contains("the profile's own patch"));
    assert!(text.contains("harness.patch.toml"));
    assert!(text.contains("--patch overlay"));
}
