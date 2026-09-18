//! What a product in another repository can build on the coding assembly.
//!
//! The harness's own `tests/external_assembly.rs` says a product outside can
//! write rows and mount them on the machine. This file says the same one layer
//! up: that a product can take **coding** — its tools, discipline, gates and
//! persona — and add rows of its own, without forking this crate.
//!
//! Like every file under `tests/`, this one is a crate of its own and reaches
//! `atomcode-coding` only through `pub`. Each entry a host outside can use gets
//! one criterion:
//!
//! | entry | who it is for |
//! |---|---|
//! | `on_harness::mount_hosted` + `HostState::plugins` | a host that wants coding driven through an `AgentHandle`, plus rows it wrote |
//! | the pieces (`plugins`, `CODING_DEFAULTS`, `coding_overlay`, `InjectProvider`, `provider_layer`) | a host that composes its own tree — its own front end, its own layer order — and still brings its own provider object |
//!
//! Every row written here is one this crate has never heard of. If one of the
//! entries stops taking them, or stops being reachable, this file says so.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_coding::on_harness::{
    self, coding_overlay, provider_layer, swap_provider, InjectProvider, Presence, ProviderSlots,
    CODING_DEFAULTS,
};
use atomcode_harness::bundle;
use atomcode_harness::seams::{AgentHandleSvc, ToolsSvc};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{AlwaysStopProvider, MockProvider};
use atomcode_kernel::tool::{Tool, ToolCall, ToolContext, ToolResult};
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin};
use serde_json::{json, Value};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("coding-ext-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

// ---- a tool and a row this crate has never heard of ------------------------

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

struct WordCountRow {
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for WordCountRow {
    fn name(&self) -> &'static str {
        "word-count"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "a word_count tool, written outside atomcode-coding"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let tools = ctx.require::<ToolsSvc>().map_err(|e| e.to_string())?;
        tools.register(Arc::new(WordCount {
            runs: self.runs.clone(),
        }))?;
        let held = tools.clone();
        let _ = ctx.effect(move || held.unregister("word_count"));
        Ok(())
    }
}

const WORD_COUNT_ROW: &str = "[[insert]]\nname = \"word-count\"\n";

/// A model that asks for `word_count` once and then answers.
fn counting_model() -> Arc<MockProvider> {
    Arc::new(MockProvider::new(vec![
        vec![StreamEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "word_count".into(),
            arguments: r#"{"text":"hello brave new world"}"#.into(),
        })],
        vec![StreamEvent::TextDelta("4 words".into())],
    ]))
}

/// One turn through the driver protocol; the text the agent streamed back.
async fn say(handle: &mut AgentHandle, text: &str) -> String {
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: text.into(),
            images: vec![],
        })
        .expect("the agent is listening");
    let mut said = String::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), handle.events.recv())
            .await
            .expect("the turn must end")
            .expect("the agent hung up mid-turn");
        match event {
            AgentEvent::TextDelta(delta) => said.push_str(&delta),
            AgentEvent::TurnComplete { .. } => return said,
            _ => {}
        }
    }
}

fn active_rows(app: &App) -> Vec<String> {
    app.tree()
        .active()
        .map(|e| {
            if e.id.is_empty() {
                e.name.clone()
            } else {
                e.id.clone()
            }
        })
        .collect()
}

/// Rows only this crate implements. Seeing them mounted is what says the
/// assembly is still coding with something added, not something else.
const CODING_OWN_ROWS: &[&str] = &["persona-atomcode", "verify-cadence", "execution-policy"];

fn assert_still_coding(app: &App) {
    let rows = active_rows(app);
    for row in CODING_OWN_ROWS {
        assert!(
            rows.iter().any(|r| r == row),
            "`{row}` is not mounted — this is no longer the coding assembly: {rows:?}"
        );
    }
}

// ---- entry 1: the one-call mount ---------------------------------------------

#[tokio::test]
async fn the_one_call_mount_takes_rows_written_elsewhere() {
    let dir = scratch("mount");
    let runs = Arc::new(AtomicUsize::new(0));

    let (mut handle, app, _slots) = on_harness::mount_hosted(
        &dir,
        Presence::Headless,
        counting_model(),
        None,
        on_harness::HostState::with_plugins(vec![Arc::new(WordCountRow { runs: runs.clone() })]),
        &[Layer::from_toml(WORD_COUNT_ROW).expect("the row's layer")],
    )
    .await
    .expect("coding plus a row written outside must mount");

    assert_still_coding(&app);
    let tools = app.context().service::<ToolsSvc>().unwrap().names();
    assert!(
        tools.iter().any(|t| t == "word_count"),
        "the outside row's tool is not in the catalog: {tools:?}"
    );

    let said = say(&mut handle, "how many words?").await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the model called `word_count`; the outside implementation must be what answered"
    );
    assert!(said.contains("4 words"), "{said:?}");
}

/// Claims the name of one of coding's own rows.
struct Impostor;

#[async_trait]
impl Plugin for Impostor {
    fn name(&self) -> &'static str {
        "persona-atomcode"
    }
    async fn apply(&self, _ctx: &Context, _config: &Value) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn a_row_that_takes_one_of_codings_names_is_refused_with_a_reason() {
    let dir = scratch("clash");
    let refused = on_harness::mount_hosted(
        &dir,
        Presence::Headless,
        Arc::new(AlwaysStopProvider::new("unused")),
        None,
        on_harness::HostState::with_plugins(vec![Arc::new(Impostor)]),
        &[],
    )
    .await;
    // An error rather than the registry's panic: the plugin list is the
    // caller's input, and a host should be told which name clashed and what to
    // do instead — replacing a row is a patch, not a second registration.
    let error = match refused {
        Ok(_) => panic!("two implementations answering to one row name must not mount"),
        Err(e) => e,
    };
    assert!(
        error.contains("persona-atomcode"),
        "the error must name the clash: {error}"
    );
}

// ---- entry 2: the pieces, for a host with its own tree -----------------------

/// Coding's rows on the harness's machine, with the provider the host built.
fn own_tree(dir: &Path, provider_id: &str) -> ConfigTree {
    let root = bundle::toml_string(&dir.to_string_lossy());
    let host = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {root} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ working_dir = {root} }}\n"
    );
    ConfigTree::from_layers([
        bundle::infra().expect("infra"),
        Layer::from_toml(CODING_DEFAULTS).expect("coding defaults"),
        Layer::from_toml(&coding_overlay(
            dir,
            &dir.join("artifacts"),
            Presence::Headless,
            "model-a",
        ))
        .expect("coding overlay"),
        provider_layer(provider_id).expect("provider layer"),
        Layer::from_toml(&host).expect("host layer"),
    ])
    .expect("tree")
}

#[tokio::test]
async fn a_host_with_its_own_tree_serves_its_own_provider_and_swaps_it() {
    let dir = scratch("pieces");
    let (slots, id) = ProviderSlots::new(Arc::new(AlwaysStopProvider::new("from model A")));

    let mut registry = atomcode_harness::plugins::catalog();
    for row in on_harness::plugins() {
        registry.register(row);
    }
    registry.register(Arc::new(InjectProvider::new(slots.clone())));

    let mut app = App::new(registry, own_tree(&dir, &id));
    app.start()
        .await
        .expect("coding's pieces plus a host-built provider must mount");
    assert_still_coding(&app);

    let mut handle = app
        .context()
        .service::<AgentHandleSvc>()
        .expect("coding's rows include the driver protocol")
        .take()
        .expect("the handle, once");

    let first = say(&mut handle, "hello").await;
    assert!(
        first.contains("from model A"),
        "the turn was not served by the provider the host built: {first:?}"
    );

    swap_provider(
        &mut app,
        &slots,
        Arc::new(AlwaysStopProvider::new("from model B")),
        "model-b",
    )
    .await
    .expect("a host that holds the slots can swap what is behind `llm`");

    let second = say(&mut handle, "again").await;
    assert!(
        second.contains("from model B") && !second.contains("from model A"),
        "the next turn must run on the swapped-in provider: {second:?}"
    );
}

// 手写链那一节删了:`parts::assemble` 与它的 `register_extra_tool` 在 2026-09-16 随
// 双引擎一起退役(`docs/handoff-collapse-dual-engine-2026-09-16.md`),装配只剩
// `prepare` + `runtime::mount` 这一条,上面两节测的正是它。
