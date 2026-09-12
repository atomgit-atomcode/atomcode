//! The execution world: containment, world swaps, and the claim that replacing a
//! provider relocates everything built on it.

use atomcode_harness::agent::OnlySession;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_capabilities::world::{Chunk, Exit, Process, Shell, SpawnError, SpawnOptions};
use atomcode_harness::seams::{FsSvc, ShellSvc, ToolsSvc};
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin};
use serde_json::Value;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-world-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A shell that records what it was asked to run and answers without spawning
/// anything. Standing in for "a different execution world" — and proof that a
/// world needs none of the local machine's platform probing to fill the seam.
#[derive(Default)]
struct RecordingShell {
    seen: Mutex<Vec<String>>,
}

/// One canned run: a single stdout chunk, then a clean exit.
struct Canned {
    chunk: Mutex<Option<Chunk>>,
}

#[async_trait]
impl Process for Canned {
    async fn next_chunk(&self) -> Option<Chunk> {
        self.chunk.lock().unwrap().take()
    }
    async fn wait(&self) -> Result<Exit, String> {
        Ok(Exit {
            code: Some(0),
            signal: None,
        })
    }
    async fn kill(&self) {}
}

#[async_trait]
impl Shell for RecordingShell {
    fn describe(&self) -> String {
        "recording (no real processes)".into()
    }
    async fn spawn(
        &self,
        command: &str,
        _options: &SpawnOptions,
    ) -> Result<Arc<dyn Process>, SpawnError> {
        self.seen.lock().unwrap().push(command.to_string());
        Ok(Arc::new(Canned {
            chunk: Mutex::new(Some(Chunk::Stdout(b"ran somewhere else".to_vec()))),
        }))
    }
}

struct RecordingShellPlugin(Arc<RecordingShell>);

#[async_trait]
impl Plugin for RecordingShellPlugin {
    fn name(&self) -> &'static str {
        "shell-recording"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["shell"]
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<ShellSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn tree(root: &Path, script: &str, extra: &[&str]) -> ConfigTree {
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
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
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

/// Approval wide open, so anything that still fails failed at the world
/// boundary rather than at a policy gate.
const YOLO: &str = "[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }";

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

#[tokio::test]
async fn the_world_fences_paths_outside_its_root() {
    let dir = scratch("fence");
    let outside = std::env::temp_dir().join("plexus-outside-the-root.txt");
    std::fs::write(&outside, "secret").unwrap();

    let app = start(tree(
        &dir,
        &script_one(
            "read_file",
            &format!(r#"{{ file_path = {:?} }}"#, outside.to_string_lossy()),
        ),
        &[YOLO],
    ))
    .await;
    run_turn(&app, "read it").await.unwrap();

    let log = app
        .context()
        .only_session()
        .unwrap();
    let text: String = log
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("outside the world's root"),
        "containment belongs to the provider, not to each tool: {text}"
    );
    assert!(
        !text.contains("secret"),
        "and it must actually stop the read"
    );
    let _ = std::fs::remove_file(outside);
}

#[tokio::test]
async fn without_a_root_the_world_is_not_fenced() {
    let dir = scratch("unfenced");
    let outside = std::env::temp_dir().join("plexus-outside-an-unfenced-root.txt");
    std::fs::write(&outside, "reachable").unwrap();

    // The test tree fences to the scratch dir; `config = {}` on a later layer
    // takes the root away again, which is what the shipped base does.
    let app = start(tree(
        &dir,
        &script_one(
            "read_file",
            &format!(r#"{{ file_path = {:?} }}"#, outside.to_string_lossy()),
        ),
        &[YOLO, "[[patch]]\nid = \"fs\"\nconfig = {}"],
    ))
    .await;
    run_turn(&app, "read it").await.unwrap();

    let text: String = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("reachable"),
        "the conversation's own agent is not fenced: {text}"
    );
    assert!(!text.contains("outside the world's root"), "{text}");
    let _ = std::fs::remove_file(outside);
}

#[tokio::test]
async fn a_read_only_world_refuses_writes_even_with_approval_wide_open() {
    let dir = scratch("readonly");
    let target = dir.join("out.txt");
    let script = script_one(
        "write_file",
        &format!(
            r#"{{ file_path = {:?}, content = "x" }}"#,
            target.to_string_lossy()
        ),
    );

    // Same tools, same approval mode, one row different.
    let app = start(tree(&dir, &script, &[YOLO])).await;
    run_turn(&app, "write").await.unwrap();
    assert!(target.exists(), "the writable world lets it through");
    std::fs::remove_file(&target).unwrap();
    drop(app);

    let read_only = format!(
        "[[patch]]\nid = \"fs\"\nname = \"fs-readonly\"\nconfig = {{ root = {:?} }}\n",
        dir.to_string_lossy()
    );
    let app = start(tree(&dir, &script, &[YOLO, read_only.as_str()])).await;
    run_turn(&app, "write").await.unwrap();
    assert!(
        !target.exists(),
        "a policy can be overridden; a world cannot be talked out of being read-only"
    );
}

#[tokio::test]
async fn replacing_the_shell_provider_relocates_bash() {
    let dir = scratch("relocate");
    let recorder = Arc::new(RecordingShell::default());
    let mut registry = plugins::catalog();
    registry.register(Arc::new(RecordingShellPlugin(recorder.clone())));

    let script = script_one("bash", r#"{ command = "echo hello" }"#);
    let swap = "[[patch]]\nid = \"shell\"\nname = \"shell-recording\"";
    let mut app = App::new(registry, tree(&dir, &script, &[YOLO, swap]));
    app.start().await.unwrap();

    // `tool-bash-world` was never patched and does not know the world moved.
    let shell = app.context().service::<ShellSvc>().unwrap();
    assert_eq!(shell.describe(), "recording (no real processes)");

    run_turn(&app, "run it").await.unwrap();
    let seen = recorder.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["echo hello".to_string()],
        "bash spawns through whatever fills `shell`, so swapping it moves bash too"
    );
    // And what the model saw came from that world, not from this machine.
    let text = transcript(&app);
    assert!(text.contains("ran somewhere else"), "{text}");
    assert!(
        !text.contains("hello\n"),
        "the local shell must not have run: {text}"
    );
}

#[tokio::test]
async fn the_local_world_really_runs_commands() {
    let dir = scratch("real-bash");
    let app = start(tree(
        &dir,
        &script_one("bash", r#"{ command = "echo from-the-world" }"#),
        &[YOLO],
    ))
    .await;
    run_turn(&app, "run it").await.unwrap();
    let text: String = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("from-the-world"), "{text}");
}

#[tokio::test]
async fn a_read_only_command_is_classified_safe_and_survives_deny_risky() {
    let dir = scratch("risk");
    // deny-risky is the shipped default; a provably read-only command passes it.
    let app = start(tree(
        &dir,
        &script_one("bash", r#"{ command = "echo safe" }"#),
        &[],
    ))
    .await;
    run_turn(&app, "run it").await.unwrap();
    let text: String = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("safe"), "{text}");
    assert!(!text.contains("Refused"), "{text}");
    drop(app);

    // A command that could mutate is refused by the same policy.
    let app = start(tree(
        &dir,
        &script_one("bash", r#"{ command = "rm -rf /tmp/nope" }"#),
        &[],
    ))
    .await;
    run_turn(&app, "run it").await.unwrap();
    let text: String = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Refused"), "{text}");
}

#[tokio::test]
async fn the_two_tool_implementations_are_mutually_exclusive() {
    let dir = scratch("exclusive");
    // Enabling the production row while the seam-routed one is live must fail
    // loudly: two rows claiming `read_file` is an ambiguous config.
    let both = "[[patch]]\nid = \"tool-fs\"\ndisabled = false";
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, &script_one("read_file", "{}"), &[both]),
    );
    let err = app.start().await.unwrap_err().to_string();
    assert!(err.contains("already registered"), "{err}");
    assert!(err.contains("read_file"), "{err}");
}

#[tokio::test]
async fn swapping_to_the_production_tools_keeps_the_model_facing_behaviour() {
    let dir = scratch("native");
    std::fs::write(dir.join("a.txt"), "content here").unwrap();
    let script = script_one("read_file", r#"{ file_path = "a.txt" }"#);

    let seam_routed = start(tree(&dir, &script, &[])).await;
    run_turn(&seam_routed, "read").await.unwrap();
    let via_seam = transcript(&seam_routed);
    drop(seam_routed);

    let native = start(tree(&dir, &script, &[bundle::NATIVE_TOOLS])).await;
    run_turn(&native, "read").await.unwrap();
    let via_native = transcript(&native);

    for text in [&via_seam, &via_native] {
        assert!(text.contains("content here"), "{text}");
        assert!(text.contains("1\t"), "both number lines from 1: {text}");
    }
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
async fn unloading_the_world_takes_its_tools_with_it() {
    let dir = scratch("unload");
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, &script_one("read_file", "{}"), &[]),
    );
    app.start().await.unwrap();
    assert!(app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .names()
        .contains(&"read_file".to_string()));

    // Removing the fs provider unloads its consumer's tools too, because the
    // consumer injected it and mounting is dependency-driven.
    app.patch(&Layer::from_toml("[[remove]]\nid = \"tool-fs-world\"").unwrap())
        .await
        .unwrap();
    let names = app.context().service::<ToolsSvc>().unwrap().names();
    assert!(!names.contains(&"read_file".to_string()), "{names:?}");
    assert!(
        names.contains(&"grep".to_string()),
        "unrelated tool rows are untouched: {names:?}"
    );
    assert!(
        app.context().service::<FsSvc>().is_some(),
        "the world itself is still mounted; only its consumer left"
    );
}

#[tokio::test]
async fn a_routed_world_mounts_no_process_path_that_bypasses_it() {
    // The production crate also ships `bash_start` / `bash_poll` / `bash_kill`.
    // They route through the same seam now, but only when handed the world —
    // `BashStartTool::default()` is this machine. A tree whose `shell` points
    // elsewhere must not offer the default form: a "read-only sandbox" with a
    // side door to the host is worse than no sandbox, because the model is told
    // it is contained. No row mounts them today; if one does, it goes through
    // `with_world` and this assertion moves to "mounted, and routed".
    let dir = scratch("no-side-door");
    let recorder = Arc::new(RecordingShell::default());
    let mut registry = plugins::catalog();
    registry.register(Arc::new(RecordingShellPlugin(recorder)));
    let swap = "[[patch]]\nid = \"shell\"\nname = \"shell-recording\"";
    let mut app = App::new(
        registry,
        tree(
            &dir,
            &script_one("bash", r#"{ command = "true" }"#),
            &[swap],
        ),
    );
    app.start().await.unwrap();

    let names = app.context().service::<ToolsSvc>().unwrap().names();
    assert!(names.contains(&"bash".to_string()), "{names:?}");
    for side_door in ["bash_start", "bash_poll", "bash_kill"] {
        assert!(
            !names.contains(&side_door.to_string()),
            "`{side_door}` spawns on this machine, not in the mounted world: {names:?}"
        );
    }
}
