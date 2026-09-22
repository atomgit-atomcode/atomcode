//! Showing a file to the person is the front end's business: present where a
//! person is, structurally absent where none is, and pointed elsewhere when the
//! person is elsewhere.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_capabilities::tools::{OpenTarget, Opener};
use atomcode_harness::profile::Profiles;
use atomcode_harness::seams::{OpenerSvc, ToolsSvc};
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
    let dir = std::env::temp_dir().join(format!("plexus-opener-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Resolve a profile with the model scripted and the world pointed at `root`.
fn tree(profile: &str, root: &std::path::Path, script: &str, extra: &[&str]) -> ConfigTree {
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let scoped = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 6, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut overlays: Vec<&str> = vec![quiet, scoped.as_str(), script];
    overlays.extend(extra);
    Profiles::builtin()
        .resolve(profile, &overlays)
        .unwrap_or_else(|e| panic!("`{profile}`: {e}"))
}

const ANSWER: &str = "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\n\
    config = { script = [ { text = \"answered\" } ] }";

fn script_open(target: &str) -> String {
    format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
         {{ text = \"showing\", calls = [ {{ name = \"open_file\", args = {{ file_path = {target:?} }} }} ] }},\n  \
         {{ text = \"done\" }},\n] }}\n"
    )
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

fn tool_names(app: &App) -> Vec<String> {
    app.context().service::<ToolsSvc>().unwrap().names()
}

#[tokio::test]
async fn open_file_exists_exactly_where_a_person_is() {
    let dir = scratch("where");
    // A terminal front end has someone at a display: the tool is there.
    {
        let profile = "repl";
        let app = start(tree(profile, &dir, ANSWER, &[])).await;
        assert!(
            tool_names(&app).contains(&"open_file".to_string()),
            "`{profile}` should be able to show files: {:?}",
            tool_names(&app)
        );
        assert!(app.context().service::<OpenerSvc>().is_some());
    }
    // Nobody is at the machine a headless, sdk, web or embedded tree runs on —
    // the tool is absent, not present-and-refusing. Web is deliberately in this
    // list: a browser tab is a person, but on *their* machine, and nothing
    // provides that yet; until something does, the honest answer is absence.
    for profile in ["headless", "oneshot", "sdk", "web", "embed"] {
        let app = start(tree(profile, &dir, ANSWER, &[])).await;
        assert!(
            !tool_names(&app).contains(&"open_file".to_string()),
            "`{profile}` has nobody to show a file to: {:?}",
            tool_names(&app)
        );
        assert!(app.context().service::<OpenerSvc>().is_none());
    }
}

#[tokio::test]
async fn the_tool_cannot_be_mounted_without_someone_to_show_things_to() {
    // The structural claim: `tool-open-file` injects `opener`, so wiring it into
    // a tree with no opener stalls the mount and names the missing seam — it
    // does not mount and then refuse every call.
    let dir = scratch("stall");
    let wired_alone = "[[insert]]\nname = \"tool-open-file\"";
    let mut app = App::new(
        plugins::catalog(),
        tree("headless", &dir, ANSWER, &[wired_alone]),
    );
    let err = app.start().await.unwrap_err().to_string();
    assert!(err.contains("tool-open-file"), "{err}");
    assert!(err.contains("opener"), "{err}");
}

/// The person is somewhere else — a browser tab, another machine. Nothing is
/// launched on the agent's host; the target is handed over.
#[derive(Default)]
struct Elsewhere(Mutex<Vec<OpenTarget>>);

#[async_trait]
impl Opener for Elsewhere {
    fn describe(&self) -> String {
        "the person's own browser".into()
    }
    async fn open(&self, target: &OpenTarget) -> Result<String, String> {
        self.0.lock().unwrap().push(target.clone());
        Ok("shown to the person".into())
    }
}

struct ElsewherePlugin(Arc<Elsewhere>);

#[async_trait]
impl Plugin for ElsewherePlugin {
    fn name(&self) -> &'static str {
        "opener-elsewhere"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["opener"]
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<OpenerSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[tokio::test]
async fn swapping_the_opener_moves_where_the_file_is_shown() {
    // The same `tool-open-file` row, never patched, presents through whatever
    // fills `opener` — the reason it is a seam and not a call to `xdg-open`.
    let dir = scratch("elsewhere");
    let elsewhere = Arc::new(Elsewhere::default());
    let mut registry = plugins::catalog();
    registry.register(Arc::new(ElsewherePlugin(elsewhere.clone())));
    let swap = "[[patch]]\nid = \"opener-local\"\nname = \"opener-elsewhere\"";
    let mut app = App::new(
        registry,
        tree(
            "repl",
            &dir,
            &script_open("https://example.com/report"),
            &[bundle::YOLO, swap],
        ),
    );
    app.start().await.unwrap();
    assert_eq!(
        app.context().service::<OpenerSvc>().unwrap().describe(),
        "the person's own browser"
    );

    run_turn(&app, "show me").await.unwrap();
    assert_eq!(
        *elsewhere.0.lock().unwrap(),
        vec![OpenTarget::Url("https://example.com/report".into())],
        "the target went to the person, not to this machine's desktop"
    );
}

#[tokio::test]
async fn unloading_the_tool_row_keeps_the_opener_for_the_front_end() {
    // The opener is the front end's, not the tool's: the WebUI's "open this
    // artifact" action goes through the same seam with no model involved. So
    // taking the model's tool away leaves the seam in place.
    let dir = scratch("unload");
    let mut app = App::new(plugins::catalog(), tree("repl", &dir, ANSWER, &[]));
    app.start().await.unwrap();
    assert!(tool_names(&app).contains(&"open_file".to_string()));
    app.patch(&Layer::from_toml("[[remove]]\nid = \"tool-open-file\"").unwrap())
        .await
        .unwrap();
    assert!(
        !tool_names(&app).contains(&"open_file".to_string()),
        "{:?}",
        tool_names(&app)
    );
    assert!(
        app.context().service::<OpenerSvc>().is_some(),
        "the person can still be shown things by the front end itself"
    );
}
