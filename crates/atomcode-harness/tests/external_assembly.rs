//! What a product in another repository can build on this crate.
//!
//! Everything under `tests/` compiles as a crate of its own, so it reaches the
//! harness only through `pub` — the same door a product outside the workspace
//! has. That makes this file the criterion for "the harness is open": it writes
//! rows the harness has never heard of, mounts them on the harness's machine
//! with a row list of its own, and checks that they did what rows do.
//!
//! The claim used to be settled by building a scratch crate by hand, which says
//! it once. Here it keeps saying it: if a piece below stops being reachable
//! (`plugins::catalog`, `bundle::infra`, `ToolBox::register`,
//! `PromptRegistry::contribute`, `Context::effect`, the `llm` seam) this file
//! stops compiling, and if a row written outside stops reaching the model it
//! goes red.
//!
//! Only the machine (`bundle::infra`) is taken whole. The product decisions are
//! the test's own row list, because that is what a product outside writes —
//! `bundle::DEFAULTS` is the harness's answer, not the one a product inherits.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::seams::{LlmSvc, SystemPromptSvc, ToolsSvc};
use atomcode_harness::{bundle, plugins, run_turn, seam_map};
use atomcode_kernel::testkit::AlwaysStopProvider;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin};
use serde_json::{json, Value};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-ext-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

// ---- a row no crate in this workspace defines ----------------------------

/// Counts the words in `text`. Records each run, so "the model asked for it"
/// and "this implementation answered" are two separate observations.
struct WordCount {
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for WordCount {
    fn name(&self) -> &str {
        "word_count"
    }
    fn description(&self) -> &str {
        "Count the whitespace-separated words in `text`."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let words = serde_json::from_str::<Value>(args)
            .ok()
            .and_then(|v| v["text"].as_str().map(|t| t.split_whitespace().count()))
            .unwrap_or(0);
        ToolResult {
            call_id: String::new(),
            content: format!("{words} words"),
            is_error: false,
            images: vec![],
        }
    }
}

/// The sentence the row contributes, so its presence in the prompt is checkable.
const FRAGMENT: &str = "You can count words with `word_count`.";

/// A tool and the prompt line that goes with it — the two contributions nearly
/// every product row makes, through the same two registries the shipped rows use.
struct WordCountRow {
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for WordCountRow {
    fn name(&self) -> &'static str {
        "word-count"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "system-prompt"]
    }
    fn description(&self) -> &'static str {
        "a word_count tool, written outside the harness"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let tools = ctx.require::<ToolsSvc>().map_err(|e| e.to_string())?;
        tools.register(Arc::new(WordCount {
            runs: self.runs.clone(),
        }))?;
        let held = tools.clone();
        let _ = ctx.effect(move || held.unregister("word_count"));

        let prompts = ctx
            .require::<SystemPromptSvc>()
            .map_err(|e| e.to_string())?;
        prompts.contribute("word-count", 50, FRAGMENT);
        let held = prompts.clone();
        let _ = ctx.effect(move || held.remove("word-count"));
        Ok(())
    }
}

/// The product's row list on the harness's machine: a tool row the harness
/// already ships, the row above, an approval row, and a scripted model.
fn product(dir: &Path, script: &str) -> ConfigTree {
    let root = bundle::toml_string(&dir.to_string_lossy());
    let rows = format!(
        "[[insert]]\nname = \"tool-fs-world\"\n\n\
         [[insert]]\nname = \"word-count\"\n\n\
         [[insert]]\nname = \"approval\"\nconfig = {{ mode = \"deny-risky\" }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 10, working_dir = {root} }}\n\n\
         {script}"
    );
    ConfigTree::from_layers([
        bundle::infra().expect("infra"),
        Layer::from_toml(&rows).expect("the product's rows"),
    ])
    .expect("tree")
}

/// Read a file with the harness's own tool, then count with the outside one.
fn read_then_count(file: &Path) -> String {
    format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
         {{ text = \"reading\", calls = [ {{ name = \"read_file\", args = {{ file_path = {file} }} }} ] }},\n  \
         {{ text = \"counting\", calls = [ {{ name = \"word_count\", args = {{ text = \"hello brave new world\" }} }} ] }},\n  \
         {{ text = \"4 words\" }},\n] }}\n",
        file = bundle::toml_string(&file.to_string_lossy())
    )
}

/// The harness's catalog plus the rows written here — how a product outside
/// makes its own rows nameable in a tree.
fn catalog_with(rows: Vec<Arc<dyn Plugin>>) -> atomcode_plexus::PluginRegistry {
    let mut catalog = plugins::catalog();
    for row in rows {
        catalog.register(row);
    }
    catalog
}

fn tool_names(app: &App) -> Vec<String> {
    app.context().service::<ToolsSvc>().unwrap().names()
}

fn prompt(app: &App) -> String {
    app.context().service::<SystemPromptSvc>().unwrap().render()
}

// ---- the criteria ---------------------------------------------------------

#[tokio::test]
async fn a_row_written_outside_the_harness_mounts_and_its_tool_runs() {
    let dir = scratch("runs");
    let file = dir.join("hello.txt");
    std::fs::write(&file, "hello brave new world").unwrap();
    let runs = Arc::new(AtomicUsize::new(0));

    let mut app = App::new(
        catalog_with(vec![Arc::new(WordCountRow { runs: runs.clone() })]),
        product(&dir, &read_then_count(&file)),
    );
    app.start()
        .await
        .expect("an assembly with a row written outside must mount");

    let defects: Vec<String> = app
        .audit_with(seam_map::HOST_CONSUMED, seam_map::HOST_PROVIDED)
        .into_iter()
        .filter(|f| f.is_defect())
        .map(|f| f.to_string())
        .collect();
    assert!(defects.is_empty(), "audit: {defects:?}");

    let names = tool_names(&app);
    assert!(
        names.iter().any(|n| n == "word_count"),
        "the outside row's tool is missing from the catalog: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "read_file"),
        "the harness's own tool row was displaced: {names:?}"
    );
    assert!(
        prompt(&app).contains(FRAGMENT),
        "the outside row's prompt line is missing"
    );

    let outcome = run_turn(&app, "how many words are in hello.txt?")
        .await
        .expect("the turn must run");
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.tool_calls, 2, "{outcome:?}");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the model called `word_count`; the outside implementation must be what answered"
    );
    assert_eq!(outcome.text, "4 words");
}

#[tokio::test]
async fn unloading_that_row_takes_its_tool_and_its_prompt_line_with_it() {
    let dir = scratch("unload");
    let runs = Arc::new(AtomicUsize::new(0));
    let mut app = App::new(
        catalog_with(vec![Arc::new(WordCountRow { runs })]),
        product(&dir, &read_then_count(&dir.join("absent.txt"))),
    );
    app.start().await.expect("mount");

    // Present first: an absence after the patch means nothing if the row never
    // contributed in the first place.
    assert!(tool_names(&app).iter().any(|n| n == "word_count"));
    assert!(prompt(&app).contains(FRAGMENT));

    app.patch(&Layer::from_toml("[[patch]]\nid = \"word-count\"\ndisabled = true\n").unwrap())
        .await
        .expect("a product's own row can be switched off at runtime");

    let names = tool_names(&app);
    assert!(
        !names.iter().any(|n| n == "word_count"),
        "the tool outlived its row: {names:?}"
    );
    assert!(
        !prompt(&app).contains(FRAGMENT),
        "the prompt line outlived its row"
    );
    assert!(
        names.iter().any(|n| n == "read_file"),
        "unloading one row took a sibling with it: {names:?}"
    );
}

/// The model adapter, written outside and put behind the harness's `llm` seam.
struct HouseModel;

const HOUSE_SAYS: &str = "answered by the house model";

#[async_trait]
impl Plugin for HouseModel {
    fn name(&self) -> &'static str {
        "llm-house"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn description(&self) -> &'static str {
        "a model adapter written outside the harness"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<LlmSvc>(Arc::new(AlwaysStopProvider::new(HOUSE_SAYS)))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[tokio::test]
async fn a_row_written_outside_the_harness_can_fill_a_seam() {
    let dir = scratch("seam");
    let tree = product(&dir, "[[patch]]\nid = \"llm\"\nname = \"llm-house\"\n");
    let rows: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(HouseModel),
        Arc::new(WordCountRow {
            runs: Arc::new(AtomicUsize::new(0)),
        }),
    ];
    let mut app = App::new(catalog_with(rows), tree);
    app.start()
        .await
        .expect("an outside row must be able to fill `llm`");

    let outcome = run_turn(&app, "hello").await.expect("the turn must run");
    assert_eq!(outcome.error, None);
    assert_eq!(
        outcome.text, HOUSE_SAYS,
        "the turn was answered by something other than the row that filled `llm`"
    );
}

// ---- a product's users' state, kept out of AtomCode's home ------------------

fn jsonl_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(jsonl_under(&path));
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            found.push(path);
        }
    }
    found
}

/// A product built on the harness is not AtomCode, and its users' sessions and
/// memory should not land in `~/.atomcode` just because nobody set an
/// environment variable. The two rows that write per-user state say where, as
/// config — and the global memory tier is both read (the first-turn injection)
/// and written (the `memory` tool), so both halves are checked against the one
/// file the row was given.
#[tokio::test]
async fn a_product_can_keep_its_users_state_out_of_the_atomcode_home() {
    use atomcode_capabilities::memory::MemoryStore;
    use atomcode_harness::agent::OnlySession;

    let dir = scratch("state");
    let state = dir.join("product-state");
    let global = state.join("memory.md");
    let sessions = state.join("sessions");
    MemoryStore::new(global.clone())
        .append_deduped("stated before this session m9q2")
        .expect("seed the product's global memory");

    let root = bundle::toml_string(&dir.to_string_lossy());
    let rows = format!(
        "[[insert]]\nname = \"memory\"\nconfig = {{ project_root = {root}, global = {global} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {sessions}, project_root = {root} }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
         {{ text = \"noting\", calls = [ {{ name = \"memory\", args = {{ action = \"remember\", content = \"learned this session m9q3\", scope = \"global\" }} }} ] }},\n  \
         {{ text = \"noted\" }},\n] }}\n",
        global = bundle::toml_string(&global.to_string_lossy()),
        sessions = bundle::toml_string(&sessions.to_string_lossy()),
    );
    let rows_written_here: Vec<Arc<dyn Plugin>> = vec![Arc::new(WordCountRow {
        runs: Arc::new(AtomicUsize::new(0)),
    })];
    let mut app = App::new(catalog_with(rows_written_here), product(&dir, &rows));
    app.start().await.expect("mount");

    let outcome = run_turn(&app, "remember something").await.expect("turn");
    assert_eq!(outcome.error, None);

    let seen: Vec<String> = app
        .context()
        .only_session()
        .expect("one session")
        .derive_messages()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert!(
        seen.iter()
            .any(|t| t.contains("stated before this session m9q2")),
        "the injection did not read the global tier the row named: {seen:?}"
    );
    assert!(
        MemoryStore::new(global)
            .load()
            .iter()
            .any(|e| e == "learned this session m9q3"),
        "the `memory` tool did not write the global tier the row named"
    );
    assert!(
        !jsonl_under(&sessions).is_empty(),
        "no session log under the root the row named"
    );

    let home = atomcode_harness::home();
    assert!(
        !MemoryStore::new(home.join("memory.md"))
            .load()
            .iter()
            .any(|e| e.contains("m9q")),
        "the product's memory leaked into $ATOMCODE_HOME"
    );
    let bucket = atomcode_config::util::stable_project_hash(&dir);
    assert!(
        jsonl_under(&home.join("sessions").join(bucket)).is_empty(),
        "the product's session leaked into $ATOMCODE_HOME"
    );
}
