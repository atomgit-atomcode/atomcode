//! The reasoning-effort row: what it does to a request, and what it must not do.
//!
//! The behaviour worth a test is not "the field gets set" on its own — it is that
//! the level is the session's rather than the model route's. A level kept on the
//! `llm` row would be wiped by the next `--model` patch, and nothing about a
//! single request would show it: this row carries no model, so swapping models
//! cannot reach it.
//!
//! Whether to reason at all is deliberately NOT tested here: that value lives on
//! the route (`thinking_type` on the `llm` row) and follows the model.

use std::sync::{Arc, Mutex};

use atomcode_kernel::provider::ReasoningEffort;
use atomcode_plexus::{App, ConfigTree, Layer, Next, Waterfall};
use serde_json::{json, Value};

use atomcode_harness::bundle;
use atomcode_harness::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};
use atomcode_harness::plugins;
use atomcode_harness::run_turn;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// Stands in for the model call: records what the request was handed. Registered
/// last, so it sees what the rows before it decided.
#[derive(Default)]
struct Seen {
    level: Option<ReasoningEffort>,
}

struct Record(Arc<Mutex<Seen>>);

#[async_trait::async_trait]
impl Waterfall<AgentRequest> for Record {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        _next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let mut seen = self.0.lock().expect("seen poisoned");
        seen.level = req.options.reasoning_effort;
        Ok(ModelResponse::default())
    }
}

/// A real tree, built the way every other harness test builds one: the base
/// bundle, the row under test, and a scripted model so a turn needs no key.
fn tree(config: Value) -> ConfigTree {
    let script = "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\n\
                  config = { script = [ { text = \"ok\" }, { text = \"ok\" }, { text = \"ok\" } ] }\n";
    // Through the TOML serializer: an inline `config = {{...}}` written by hand
    // would have to be valid TOML, and JSON is not (its keys are quoted).
    let row = toml::to_string(&json!({
        "insert": [{ "name": "reasoning-effort", "config": config }]
    }))
    .expect("serialize the row");
    ConfigTree::from_layers(vec![
        atomcode_coding::on_harness::base_layer(),
        atomcode_coding::on_harness::headless_patch(),
        Layer::from_toml(script).expect("script layer"),
        Layer::from_toml(&row).expect("row layer"),
    ])
    .expect("compose")
}

/// Mount the tree and run one turn; report what the model request carried.
async fn after_one_turn(config: Value) -> Seen {
    let (app, seen) = mount(config).await;
    run_turn(&app, "go").await.expect("one turn");
    let out = seen.lock().expect("seen poisoned");
    Seen { level: out.level }
}

/// Mounted, with the recorder attached — for the test that drives two turns and
/// swaps the model in between.
async fn mount(config: Value) -> (App, Arc<Mutex<Seen>>) {
    let mut app = App::new(plugins::catalog(), tree(config));
    app.start().await.expect("mount");
    let seen = Arc::new(Mutex::new(Seen::default()));
    // A `Disposable` that is never dropped would unregister the listener, so it
    // is leaked deliberately: the app outlives it here either way.
    std::mem::forget(
        app.context()
            .on_waterfall::<AgentRequest>(Arc::new(Record(seen.clone())), false),
    );
    (app, seen)
}

/// No opinion on either value: the request must go out untouched, so the
/// endpoint's own default (thinking ON at `high`, on DeepSeek) stands.
#[tokio::test]
async fn an_unconfigured_row_says_nothing() {
    let seen = after_one_turn(json!({})).await;
    assert_eq!(seen.level, None);
}

/// The row sets a strength; it must not touch anything else on the request.
#[tokio::test]
async fn a_level_travels() {
    let seen = after_one_turn(json!({ "level": "high" })).await;
    assert_eq!(seen.level, Some(ReasoningEffort::High));
}

#[tokio::test]
async fn every_level_the_ladder_names_gets_through() {
    for level in ["low", "medium", "high", "xhigh", "max"] {
        let seen = after_one_turn(json!({ "level": level })).await;
        assert_eq!(
            seen.level,
            ReasoningEffort::from_config(Some(level)),
            "`{level}` must arrive as itself"
        );
    }
}

/// An unknown level is "no opinion", not a failure to start: a typo must never
/// be able to keep a session from opening, and must never become a value the
/// gateway is stuck with.
#[tokio::test]
async fn an_unknown_level_is_simply_not_applied() {
    for typo in ["hihg", "", "off"] {
        let seen = after_one_turn(json!({ "level": typo })).await;
        assert_eq!(seen.level, None, "`{typo}` is not a level");
    }
}

/// **The requirement this design exists for.** Replacing the `llm` row — which
/// is exactly what `--model` and `/model` do — must not disturb the level. If it
/// lived on that row it would be gone here, and this is the test that would
/// catch it.
#[tokio::test]
async fn switching_the_model_keeps_the_level() {
    let (mut app, seen) = mount(json!({ "level": "high" })).await;

    run_turn(&app, "go").await.expect("first turn");
    assert_eq!(
        seen.lock().expect("seen poisoned").level,
        Some(ReasoningEffort::High),
        "set before any swap"
    );

    // A different model route, patched the way `--model` patches it: the row is
    // replaced wholesale, which is what would take a tag-along field with it.
    let layer = Layer::from_toml(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\n\
         config = { script = [ { text = \"ok\" }, { text = \"ok\" } ] }\n",
    )
    .expect("a layer");
    app.patch(&layer).await.expect("swap the model");

    seen.lock().expect("seen poisoned").level = None;
    run_turn(&app, "go again").await.expect("second turn");
    assert_eq!(
        seen.lock().expect("seen poisoned").level,
        Some(ReasoningEffort::High),
        "the level is the session's, so a model switch must not reset it"
    );
}
