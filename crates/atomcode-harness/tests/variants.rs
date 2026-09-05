//! The four product variants: one base, four specializations, and the
//! guarantees each one actually holds.
//!
//! A variant is a composition, not a fork. What makes that claim worth testing
//! is that the differences have to be *real* — a read-only reviewer that can be
//! talked into writing by a misconfigured approval row is not read-only, it is
//! a coding agent with a strong prompt.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::profile::Profiles;
use atomcode_harness::seams::{
    FindingsSvc, SessionSvc, ShellSvc, SubprocessSvc, SystemPromptSvc, ToolsSvc, UiSvc,
};
use atomcode_harness::{plugins, run_turn};
use atomcode_plexus::{App, ConfigTree};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

const VARIANTS: &[&str] = &["longcode", "longcode-air", "code-security", "code-review"];

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-var-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A variant tree with the model scripted, the world pointed at `root`, and
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

#[tokio::test]
async fn every_variant_mounts_and_audits_clean() {
    let dir = scratch("mount");
    for variant in VARIANTS {
        let mut app = App::new(plugins::catalog(), tree(variant, &dir, &says("ok"), &[]));
        app.start()
            .await
            .unwrap_or_else(|e| panic!("`{variant}` does not mount: {e}"));
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
            "`{variant}`: {}",
            findings
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
}

#[tokio::test]
async fn each_variant_gets_its_own_prompt() {
    let dir = scratch("prompts");
    async fn prompt(dir: &std::path::Path, variant: &str) -> String {
        let app = start(tree(variant, dir, &says("ok"), &[])).await;
        app.context().service::<SystemPromptSvc>().unwrap().render()
    }

    assert!(prompt(&dir, "longcode").await.contains("coding agent"));
    assert!(prompt(&dir, "longcode-air").await.contains("coding agent"));
    assert!(prompt(&dir, "code-security")
        .await
        .contains("security reviewer"));
    assert!(prompt(&dir, "code-review").await.contains("Reviewer"));

    // And the coding prompt is *gone* from the audit variants, not merely
    // outranked — a reviewer told it may edit will try to.
    assert!(!prompt(&dir, "code-security").await.contains("coding agent"));
    assert!(!prompt(&dir, "code-review").await.contains("coding agent"));
}

// ---- the read-only guarantee -------------------------------------------

#[tokio::test]
async fn an_audit_variant_cannot_write_even_with_approval_wide_open() {
    for variant in ["code-security", "code-review"] {
        let dir = scratch("readonly");
        let target = dir.join("breach.txt");
        let script = format!(
            "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
             {{ text = \"writing\", calls = [ {{ name = \"write_file\", args = {{ file_path = {:?}, content = \"x\" }} }} ] }},\n  \
             {{ text = \"done\" }},\n] }}\n",
            target.to_string_lossy()
        );
        let app = start(tree(
            variant,
            &dir,
            &script,
            &[atomcode_harness::bundle::YOLO],
        ))
        .await;
        run_turn(&app, "audit").await.unwrap();

        assert!(
            !target.exists(),
            "`{variant}` wrote a file with approval wide open — the guarantee is a prompt, \
             not a boundary"
        );
        assert!(transcript(&app).contains("read-only"));
    }
}

#[tokio::test]
async fn an_audit_variant_has_no_process_execution_at_all() {
    let dir = scratch("no-shell");
    for variant in ["code-security", "code-review"] {
        let app = start(tree(variant, &dir, &says("ok"), &[])).await;
        let ctx = app.context();
        // Not "bash is denied" — the capability is not in the tree. A denied
        // tool is one patch away from being allowed; an absent world is not.
        assert!(
            ctx.service::<ShellSvc>().is_none(),
            "`{variant}` still has a shell provider"
        );
        assert!(
            ctx.service::<SubprocessSvc>().is_none(),
            "`{variant}` still has a process provider"
        );
        assert!(!tools_of(&app).contains(&"bash".to_string()));
    }
}

#[tokio::test]
async fn the_coding_variants_can_write_and_run() {
    let dir = scratch("can-write");
    let target = dir.join("out.txt");
    for variant in ["longcode", "longcode-air"] {
        let _ = std::fs::remove_file(&target);
        let script = format!(
            "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
             {{ text = \"writing\", calls = [ {{ name = \"write_file\", args = {{ file_path = {:?}, content = \"{variant}\" }} }} ] }},\n  \
             {{ text = \"done\" }},\n] }}\n",
            target.to_string_lossy()
        );
        let app = start(tree(
            variant,
            &dir,
            &script,
            &[atomcode_harness::bundle::YOLO],
        ))
        .await;
        run_turn(&app, "write").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            *variant,
            "`{variant}` is the variant that changes code and must be able to"
        );
        assert!(app.context().service::<ShellSvc>().is_some());
    }
}

// ---- what distinguishes the variants ------------------------------------

#[tokio::test]
async fn air_is_the_same_agent_with_fewer_capabilities_mounted() {
    let dir = scratch("air");
    let full = start(tree("longcode", &dir, &says("ok"), &[])).await;
    let air = start(tree("longcode-air", &dir, &says("ok"), &[])).await;

    let full_tools = tools_of(&full);
    let air_tools = tools_of(&air);
    assert!(
        air_tools.len() < full_tools.len(),
        "Air should mount strictly less: {air_tools:?} vs {full_tools:?}"
    );
    // Air keeps what the work needs and drops the cost multipliers.
    for kept in [
        "read_file",
        "write_file",
        "edit_file",
        "grep",
        "bash",
        "list_symbols",
    ] {
        assert!(air_tools.contains(&kept.to_string()), "Air lost `{kept}`");
    }
    for dropped in ["trace_callers", "blast_radius", "task", "use_skill"] {
        assert!(
            !air_tools.contains(&dropped.to_string()),
            "Air still mounts `{dropped}`"
        );
    }
    // Same loop, same session model, same policy engine — only quantities differ.
    for shared in ["agent-loop", "sessions", "approval", "compaction"] {
        assert!(air.context().service_names().contains(&shared));
    }
}

#[tokio::test]
async fn the_audit_variants_mount_the_finding_sink() {
    let dir = scratch("findings");
    for variant in ["code-security", "code-review"] {
        let app = start(tree(variant, &dir, &says("ok"), &[])).await;
        assert!(tools_of(&app).contains(&"report_finding".to_string()));
        assert!(
            app.context().service::<FindingsSvc>().is_some(),
            "`{variant}` has the tool but nowhere for it to report"
        );
    }
    // And the coding variants do not: their product is a changed tree.
    let coding = start(tree("longcode", &dir, &says("ok"), &[])).await;
    assert!(!tools_of(&coding).contains(&"report_finding".to_string()));
    assert!(coding.context().service::<FindingsSvc>().is_none());
}

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
    let app = start(tree("code-security", &dir, script, &[])).await;
    run_turn(&app, "audit a.rs").await.unwrap();

    let findings = app.context().service::<FindingsSvc>().unwrap().all();
    assert_eq!(findings.len(), 1, "the model reported one and it must land");
    assert_eq!(findings[0].title, "unchecked input");
    assert_eq!(findings[0].priority, "P1");
    assert_eq!(findings[0].file_path, "a.rs");
    assert!((findings[0].confidence - 0.8).abs() < 0.01);
}

#[tokio::test]
async fn security_gets_the_graph_layer_because_reachability_is_the_job() {
    let dir = scratch("graph");
    let security = start(tree("code-security", &dir, &says("ok"), &[])).await;
    for tool in ["find_references", "trace_callers", "blast_radius"] {
        assert!(
            tools_of(&security).contains(&tool.to_string()),
            "a security audit without `{tool}` cannot establish a path to a sink"
        );
    }
    // Review reads a diff and its surroundings; a whole-repo index is usually
    // more than that needs, so it is off by default.
    let review = start(tree("code-review", &dir, &says("ok"), &[])).await;
    assert!(!tools_of(&review).contains(&"blast_radius".to_string()));
    assert!(
        tools_of(&review).contains(&"find_references".to_string()),
        "but the stateless symbol tools stay"
    );
}

// ---- the specialization is orthogonal to the front end ------------------

#[tokio::test]
async fn any_variant_runs_behind_any_front_end() {
    let dir = scratch("orthogonal");
    for variant in VARIANTS {
        for ui in ["oneshot", "repl", "tui", "web", "sdk", "quiet"] {
            let overlay = atomcode_harness::bundle::ui_overlay(ui);
            let mut app = App::new(
                plugins::catalog(),
                tree(variant, &dir, &says("ok"), &[overlay.as_str()]),
            );
            app.start()
                .await
                .unwrap_or_else(|e| panic!("`{variant}` + `{ui}`: {e}"));
            assert!(
                app.context().service::<UiSvc>().is_some(),
                "`{variant}` + `{ui}` mounted no front end"
            );
        }
    }
}

#[tokio::test]
async fn swapping_the_front_end_does_not_change_the_specialization() {
    let dir = scratch("same-spec");
    let mut catalogs = Vec::new();
    let mut prompts = Vec::new();
    for ui in ["oneshot", "web", "sdk"] {
        let overlay = atomcode_harness::bundle::ui_overlay(ui);
        let app = start(tree(
            "code-security",
            &dir,
            &says("ok"),
            &[overlay.as_str()],
        ))
        .await;
        catalogs.push(tools_of(&app));
        prompts.push(app.context().service::<SystemPromptSvc>().unwrap().render());
    }
    assert!(
        catalogs.windows(2).all(|w| w[0] == w[1]),
        "the front end must not change what the auditor can reach"
    );
    assert!(prompts.windows(2).all(|w| w[0] == w[1]));
}
