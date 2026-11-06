//! The execution world: containment, world swaps, and the claim that replacing a
//! provider relocates everything built on it.

use atomcode_harness::agent::OnlySession;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_capabilities::world::{Chunk, Exit, Process, Shell, SpawnError, SpawnOptions};
use atomcode_harness::seams::{FsSvc, ShellSvc, ToolsSvc};
use atomcode_harness::{bundle, run_turn};
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
    let mut layers = vec![
        atomcode_coding::on_harness::base_layer(),
        atomcode_coding::on_harness::headless_patch(),
    ];
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
    let mut app = App::new(atomcode_coding::on_harness::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

#[tokio::test]
async fn read_file_uses_extra_read_grants_without_opening_the_write_boundary() {
    let dir = scratch("skill-resource-tool");
    let bundle = tempfile::tempdir().unwrap();
    let resource = bundle.path().join("docs-zh.json");
    std::fs::write(&resource, "offline-resource-marker").unwrap();
    let read_patch = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {:?}, also_readable = [{:?}] }}",
        dir.to_string_lossy(),
        bundle.path().to_string_lossy()
    );
    let app = start(tree(
        &dir,
        &script_one(
            "read_file",
            &format!(r#"{{ file_path = {:?} }}"#, resource.to_string_lossy()),
        ),
        &[YOLO, &read_patch],
    ))
    .await;
    run_turn(&app, "read the skill resource").await.unwrap();
    let text = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("offline-resource-marker"), "{text}");
    let fs = app.context().service::<FsSvc>().unwrap();
    assert!(fs.write_text(&resource, "changed").await.is_err());
    assert_eq!(
        std::fs::read_to_string(resource).unwrap(),
        "offline-resource-marker"
    );
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

    let log = app.context().only_session().unwrap();
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
    let mut registry = atomcode_coding::on_harness::catalog();
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
        atomcode_coding::on_harness::catalog(),
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
        atomcode_coding::on_harness::catalog(),
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
    // `bash_start` / `bash_poll` / `bash_kill` are mounted beside `bash` — the
    // way to leave a dev server running. `BashStartTool::default()` is this
    // machine, and a tree whose `shell` points elsewhere must not offer that: a
    // "read-only sandbox" with a side door to the host is worse than no sandbox,
    // because the model is told it is contained. So they are mounted, and
    // routed: a background job starts in the world `shell` names.
    let dir = scratch("no-side-door");
    let recorder = Arc::new(RecordingShell::default());
    let mut registry = atomcode_coding::on_harness::catalog();
    registry.register(Arc::new(RecordingShellPlugin(recorder.clone())));
    let swap = "[[patch]]\nid = \"shell\"\nname = \"shell-recording\"";
    let mut app = App::new(
        registry,
        tree(
            &dir,
            &script_one("bash_start", r#"{ command = "npm run dev" }"#),
            &[YOLO, swap],
        ),
    );
    app.start().await.unwrap();

    let names = app.context().service::<ToolsSvc>().unwrap().names();
    for tool in ["bash", "bash_start", "bash_poll", "bash_kill"] {
        assert!(names.contains(&tool.to_string()), "`{tool}`: {names:?}");
    }
    run_turn(&app, "start the server").await.unwrap();
    assert_eq!(
        *recorder.seen.lock().unwrap(),
        vec!["npm run dev".to_string()],
        "the background job started in the mounted world, not on this machine"
    );
}

struct PatchEightK(std::sync::atomic::AtomicBool);
#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for PatchEightK {
    async fn handle(
        &self,
        request: &mut atomcode_harness::events::ModelRequest,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
    ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
    {
        request.options.max_tokens = Some(8192);
        let reference = request.messages.iter().rev().find_map(|message| {
            serde_json::from_str::<Value>(&message.text)
                .ok()
                .filter(|value| value.get("artifact_id").is_some())
        });
        let mut response = next.run(request).await?;
        if let Some(reference) = reference {
            if !self.0.swap(true, Ordering::SeqCst) {
                response.tool_calls = vec![atomcode_kernel::tool::ToolCall { id: "apply-imported-patch".into(), name: "apply_patch".into(), arguments: serde_json::json!({"artifact_id":reference["artifact_id"],"file_paths":reference["file_paths"]}).to_string() }];
            }
        }
        for call in &response.tool_calls {
            assert!(
                call.arguments.len() < 1024,
                "patch body must never be model output"
            );
        }
        Ok(response)
    }
}

#[tokio::test]
async fn native_patch_import_and_reference_application_do_not_reemit_patch_body() {
    let dir = scratch("lossless-patch");
    let mut patch = String::new();
    let mut expected = Vec::new();
    for file in 0..5 {
        let before = (0..400)
            .map(|line| format!("old-{file}-{line}\n"))
            .collect::<String>();
        let after = (0..400)
            .map(|line| format!("new-{file}-{line}\n"))
            .collect::<String>();
        std::fs::write(dir.join(format!("f{file}")), before).unwrap();
        patch.push_str(&format!("--- a/f{file}\n+++ b/f{file}\n"));
        for hunk in 0..4 {
            patch.push_str(&format!(
                "@@ -{},100 +{},100 @@\n",
                hunk * 100 + 1,
                hunk * 100 + 1
            ));
            for line in hunk * 100..(hunk + 1) * 100 {
                patch.push_str(&format!("-old-{file}-{line}\n"));
            }
            for line in hunk * 100..(hunk + 1) * 100 {
                patch.push_str(&format!("+new-{file}-{line}\n"));
            }
        }
        expected.push(after);
    }
    std::fs::write(dir.join("user.patch"), patch).unwrap();
    let patch_rows = format!(
        r#"[[insert]]
name = "tool-output-artifact"
config = {{ dir = {:?} }}
[[insert]]
name = "tool-patch"
"#,
        dir.join("artifacts").to_string_lossy()
    );
    let app = start(tree(
        &dir,
        &script_one("import_patch", r#"{ file_path = "user.patch" }"#),
        &[YOLO, &patch_rows],
    ))
    .await;
    let _budget = app
        .context()
        .on_waterfall::<atomcode_harness::events::AgentRequest>(
            Arc::new(PatchEightK(std::sync::atomic::AtomicBool::new(false))),
            true,
        );
    run_turn(&app, "Import the provided patch without copying its body")
        .await
        .unwrap();
    let messages = app.context().only_session().unwrap().derive_messages();
    let record = messages
        .iter()
        .find_map(|message| {
            serde_json::from_str::<Value>(&message.text)
                .ok()
                .filter(|value| value.get("artifact_id").is_some())
        })
        .unwrap_or_else(|| {
            panic!(
                "import returns artifact reference: {:?}",
                messages.iter().map(|m| &m.text).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        record["file_paths"],
        serde_json::json!(["f0", "f1", "f2", "f3", "f4"])
    );
    let applied = messages
        .iter()
        .filter(|message| {
            message
                .tool_calls
                .iter()
                .any(|call| call.name == "apply_patch")
        })
        .count();
    assert_eq!(
        applied, 1,
        "one reference application through runtime routing"
    );
    for (file, after) in expected.iter().enumerate() {
        assert_eq!(
            std::fs::read_to_string(dir.join(format!("f{file}"))).unwrap(),
            *after
        );
    }
}
