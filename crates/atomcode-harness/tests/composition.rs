//! What the *engine* guarantees about any assembly built on it.
//!
//! This file used to test four product variants. The products moved out — a
//! specialization is rows a separate assembly contributes, and this crate is the
//! catalog and the base, not the place products live. What is left is the part
//! that had to stay: the claims a product *relies* on and cannot verify for
//! itself.
//!
//! The load-bearing one is that a guarantee must be a boundary rather than a
//! prompt. A read-only agent that can be talked into writing by a misconfigured
//! approval row is not read-only, it is a coding agent with a strong opinion —
//! so the read-only tests below deliberately open approval all the way and still
//! expect the write to fail.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::profile::Profiles;
use atomcode_harness::seams::{
    FindingsSvc, SessionSvc, ShellSvc, SystemPromptSvc, ToolsSvc, UiSvc,
};
use atomcode_harness::{plugins, run_turn};
use atomcode_plexus::{App, ConfigTree};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// A representative spread rather than every shipped profile: the claim is that
/// the front end is orthogonal to the rest of the tree, and the mount-everything
/// sweep below already covers the full list.
const SPREAD: &[&str] = &["oneshot", "repl", "full", "plan", "headless"];

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-comp-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A profile's tree with the model scripted, the world pointed at `root`, and
/// external servers left alone — a test must not reach the network.
fn tree(profile: &str, root: &std::path::Path, script: &str, extra: &[&str]) -> ConfigTree {
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let base = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"mcp\"\ndisabled = true\n\n\
         [[patch]]\nid = \"tool-web\"\ndisabled = true\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 10, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut overlays: Vec<&str> = vec![base.as_str(), script];
    overlays.extend(extra);
    Profiles::builtin()
        .resolve(profile, &overlays)
        .unwrap_or_else(|e| panic!("`{profile}`: {e}"))
}

fn says(text: &str) -> String {
    format!("[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {{ text = \"{text}\" }} ] }}\n")
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

fn tools_of(app: &App) -> Vec<String> {
    let mut names = app.context().service::<ToolsSvc>().unwrap().names();
    names.sort();
    names
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

/// Write a file through the model, and say whether it landed.
fn write_script(target: &std::path::Path, contents: &str) -> String {
    format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
         {{ text = \"writing\", calls = [ {{ name = \"write_file\", args = {{ file_path = {:?}, content = {contents:?} }} }} ] }},\n  \
         {{ text = \"done\" }},\n] }}\n",
        target.to_string_lossy()
    )
}

// ---- every shipped profile is a real assembly ---------------------------

#[tokio::test]
async fn every_shipped_profile_mounts_and_audits_clean() {
    let dir = scratch("mount");
    for profile in Profiles::builtin().names() {
        let mut app = App::new(plugins::catalog(), tree(profile, &dir, &says("ok"), &[]));
        app.start()
            .await
            .unwrap_or_else(|e| panic!("`{profile}` does not mount: {e}"));
        let findings: Vec<_> = app
            .audit_with(
                atomcode_harness::seam_map::HOST_CONSUMED,
                atomcode_harness::seam_map::HOST_PROVIDED,
            )
            .into_iter()
            .filter(|f| f.is_defect())
            .collect();
        assert!(
            findings.is_empty(),
            "`{profile}`: {}",
            findings
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
}

// ---- the read-only guarantee is a world, not a policy -------------------

#[tokio::test]
async fn a_read_only_world_refuses_writes_even_with_approval_wide_open() {
    let dir = scratch("readonly");
    let target = dir.join("breach.txt");
    let app = start(tree(
        "repl",
        &dir,
        &write_script(&target, "x"),
        &[
            atomcode_harness::bundle::READ_ONLY,
            // Deliberately last: nothing is denied any more, so whatever stops
            // the write is the world and not the policy.
            atomcode_harness::bundle::YOLO,
        ],
    ))
    .await;
    run_turn(&app, "write it").await.unwrap();

    assert!(
        !target.exists(),
        "a file was written with approval wide open — the guarantee is a prompt, not a boundary"
    );
    assert!(transcript(&app).contains("read-only"));
}

/// Which mounted read tools actually respect the `fs` fence.
///
/// A fenced world is only a boundary for the tools that *go through* it. A tool
/// that reaches the disk on its own is outside the fence no matter what the `fs`
/// row says — and the seam map cannot tell you that, because a tool which never
/// declared `fs` as a dependency looks innocent.
///
/// So this measures rather than assumes: every read tool gets a path that
/// escapes the fence, and the ones that come back with the canary are named.
async fn tools_that_escape_the_fence() -> Vec<&'static str> {
    let dir = scratch("fence");
    let inside = dir.join("inside");
    std::fs::create_dir_all(&inside).expect("fence dir");
    // Outside the fence, with a name and a body that are both unmistakable.
    std::fs::write(dir.join("canary.txt"), "CANARY-e7f19a\n").expect("canary");
    std::fs::write(inside.join("ordinary.txt"), "nothing to see\n").expect("decoy");

    // Each probe points a tool at the parent of its own world, and says what
    // would prove the fence was crossed.
    //
    // The marker has to be chosen per probe. A tool asked for `../canary.txt`
    // names that path in its *refusal* too, so for those the only honest proof
    // is the file's contents; a tool asked for `..` can only produce the name by
    // having actually listed it. Getting this wrong reports a correct denial as
    // a leak — which it did, the first time this was run.
    //
    // `grep`'s pattern is the canary's PREFIX, so an argument echoed back into
    // the transcript cannot be mistaken for a match on the full string.
    let probes: &[(&str, &str, &str)] = &[
        (
            "read_file",
            r#"{ name = "read_file", args = { file_path = "../canary.txt" } }"#,
            "CANARY-e7f19a",
        ),
        (
            "list_directory",
            r#"{ name = "list_directory", args = { path = "..", depth = 1 } }"#,
            "canary.txt",
        ),
        (
            "grep",
            r#"{ name = "grep", args = { pattern = "CANARY", path = ".." } }"#,
            "CANARY-e7f19a",
        ),
        (
            "glob",
            r#"{ name = "glob", args = { pattern = "*.txt", path = ".." } }"#,
            "canary.txt",
        ),
    ];

    let mut escaped = Vec::new();
    for (tool, call, leak) in probes {
        let script = format!(
            "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
             {{ text = \"looking\", calls = [ {call} ] }},\n  {{ text = \"done\" }},\n] }}\n"
        );
        // `yolo` so nothing is refused by policy: whatever contains the tool here
        // is the world, which is the only thing being measured.
        let app = start(tree(
            "headless",
            &inside,
            &script,
            &[atomcode_harness::bundle::YOLO],
        ))
        .await;
        run_turn(&app, "look around").await.unwrap();

        if transcript(&app).contains(leak) {
            escaped.push(*tool);
        }
    }
    escaped
}

#[tokio::test]
async fn the_fence_holds_for_the_tools_that_go_through_the_world() {
    // Encoded as an exact set rather than a count, and deliberately not
    // `#[ignore]`d: an ignored test is one nobody runs, and a bare count does not
    // say *which* tool started leaking.
    //
    // `grep` and `glob` traverse with `ignore::WalkBuilder`, which reads the real
    // disk and never consults the `fs` row. Routing only their existence check
    // would be worse than leaving them alone — the seam map would then report
    // them as world-routed while the traversal still went around it.
    //
    // The fix is a decision, not a patch, and it belongs with whoever makes it:
    //   (a) give the world a search primitive, so a remote world runs its own
    //       ripgrep and returns matches — the shape a sandbox wants anyway; or
    //   (b) do not mount grep/glob in a non-local world, the way `open_file`
    //       already cannot be mounted in one.
    //
    // Until then this test's job is to make sure the list does not grow, and to
    // fail loudly on the happy day it shrinks.
    const KNOWN_TO_ESCAPE: &[&str] = &["grep", "glob"];

    let escaped = tools_that_escape_the_fence().await;

    assert_eq!(
        escaped, KNOWN_TO_ESCAPE,
        "the set of tools that read outside the `fs` fence changed.\n\
         · a tool ADDED to the list is a new containment hole — route it through the world;\n\
         · a tool REMOVED means it is contained now — delete it from KNOWN_TO_ESCAPE;\n\
         observed: {escaped:?}, expected: {KNOWN_TO_ESCAPE:?}"
    );
}

#[tokio::test]
async fn dropping_the_process_rows_removes_the_capability_not_just_the_tool() {
    let dir = scratch("no-shell");
    let no_processes = "\
[[patch]]
id = \"tool-bash-world\"
disabled = true

[[patch]]
id = \"shell\"
disabled = true
";
    let app = start(tree("repl", &dir, &says("ok"), &[no_processes])).await;
    let ctx = app.context();
    // Not "bash is denied" — the capability is not in the tree. A denied tool is
    // one patch away from being allowed; an absent world is not.
    assert!(
        ctx.service::<ShellSvc>().is_none(),
        "a shell provider is still mounted"
    );
    assert!(!tools_of(&app).contains(&"bash".to_string()));
}

/// The positive control for the two above: without it, both would pass just as
/// well against a tree where writing never worked at all.
#[tokio::test]
async fn the_default_world_can_write_and_run() {
    let dir = scratch("can-write");
    let target = dir.join("out.txt");
    let app = start(tree(
        "repl",
        &dir,
        &write_script(&target, "landed"),
        &[atomcode_harness::bundle::YOLO],
    ))
    .await;
    run_turn(&app, "write it").await.unwrap();

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "landed");
    assert!(app.context().service::<ShellSvc>().is_some());
}

// ---- the findings seam --------------------------------------------------

#[tokio::test]
async fn a_reported_finding_reaches_the_sink() {
    let dir = scratch("report");
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
    let script = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [
  { text = "found one", calls = [ { name = "report_finding", args = { title = "unchecked input", body = "the argument reaches the sink unvalidated", priority = "P1", confidence = 0.8, file_path = "a.rs", line_start = 1, line_end = 1, suggestion = "validate it" } } ] },
  { text = "that is all" },
] }
"#;
    // The row is in the catalog and no shipped profile mounts it — which is the
    // arrangement an audit product relies on, so it is the arrangement to test.
    let app = start(tree(
        "headless",
        &dir,
        script,
        &[
            "[[insert]]\nname = \"tool-report-finding\"\n",
            atomcode_harness::bundle::YOLO,
        ],
    ))
    .await;
    assert!(tools_of(&app).contains(&"report_finding".to_string()));
    run_turn(&app, "audit a.rs").await.unwrap();

    let findings = app.context().service::<FindingsSvc>().unwrap().all();
    assert_eq!(findings.len(), 1, "the model reported one and it must land");
    assert_eq!(findings[0].title, "unchecked input");
    assert_eq!(findings[0].priority, "P1");
    assert_eq!(findings[0].file_path, "a.rs");
    assert!((findings[0].confidence - 0.8).abs() < 0.01);
}

#[tokio::test]
async fn without_the_row_there_is_no_tool_and_nowhere_to_report() {
    let dir = scratch("no-sink");
    let app = start(tree("headless", &dir, &says("ok"), &[])).await;
    assert!(!tools_of(&app).contains(&"report_finding".to_string()));
    assert!(app.context().service::<FindingsSvc>().is_none());
}

// ---- the front end is orthogonal to the rest of the tree ----------------

#[tokio::test]
async fn any_profile_runs_behind_any_front_end() {
    let dir = scratch("orthogonal");
    for profile in SPREAD {
        for ui in ["oneshot", "repl", "tui", "web", "sdk", "quiet"] {
            let overlay = atomcode_harness::bundle::ui_overlay(ui);
            let mut app = App::new(
                plugins::catalog(),
                tree(profile, &dir, &says("ok"), &[overlay.as_str()]),
            );
            app.start()
                .await
                .unwrap_or_else(|e| panic!("`{profile}` + `{ui}`: {e}"));
            assert!(
                app.context().service::<UiSvc>().is_some(),
                "`{profile}` + `{ui}` mounted no front end"
            );
        }
    }
}

#[tokio::test]
async fn swapping_the_front_end_does_not_change_the_agent() {
    let dir = scratch("same-agent");
    let mut catalogs = Vec::new();
    let mut prompts = Vec::new();
    for ui in ["oneshot", "web", "sdk"] {
        let overlay = atomcode_harness::bundle::ui_overlay(ui);
        let app = start(tree("plan", &dir, &says("ok"), &[overlay.as_str()])).await;
        catalogs.push(tools_of(&app));
        prompts.push(app.context().service::<SystemPromptSvc>().unwrap().render());
    }
    assert!(
        catalogs.windows(2).all(|w| w[0] == w[1]),
        "the front end must not change what the agent can reach"
    );
    assert!(prompts.windows(2).all(|w| w[0] == w[1]));
}
