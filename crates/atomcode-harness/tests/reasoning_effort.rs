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
        bundle::base().expect("base bundle"),
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

// ---- the rule this tree is built on ----------------------------------------

/// Every offender under `src/`, with `model_source.rs` as the one allowed file.
///
/// Narrow on purpose, in one direction: `current_dir()`, `temp_dir()` and
/// `args()` are facts about the process, not places configuration comes from.
fn offenders_in_src(flag: impl Fn(&str) -> bool) -> Vec<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // The one module allowed to know where configuration lives.
            if path.file_name().and_then(|n| n.to_str()) == Some("model_source.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            for (n, line) in text.lines().enumerate() {
                // Comments and doc prose explain the rule; they do not break it.
                let code = line.split("//").next().unwrap_or("");
                if flag(code) {
                    offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }
    }
    offenders
}

/// **Where configuration lives is decided in one place, and it is `model_source`.**
///
/// Before it existed, four rows each read the environment and the config file
/// with their own fallbacks and their own error text — the same gateway resolved
/// two ways in one process.
///
/// The first version of this check looked for `std::env::var` and nothing else,
/// and a public `env_var` helper walked straight through it: the *spelling* had
/// moved into one module while the *decision* stayed at every call site. The
/// second version flagged the variable *names*, which is a false positive — a
/// help line telling someone to `export ATOMCODE_API_KEY` is documentation.
///
/// So it flags the two things that actually are resolution:
///
/// 1. any way of reaching the environment, whatever helper is used;
/// 2. a default variable name chosen at a call site
///    (`…unwrap_or("ATOMCODE_API_KEY")`), which is a policy decision made far
///    from the policy.
///
/// Not covered, deliberately: a bare name inside prose, and a `const` declared
/// elsewhere. The first is documentation; the second would need a name match
/// that cannot tell prose from code.
#[test]
fn nothing_outside_the_source_module_reaches_the_environment() {
    let offenders = offenders_in_src(|code| {
        let reaches_env = ["env::var", "env::vars", "var_os(", "env_var("]
            .iter()
            .any(|needle| code.contains(needle));
        // A default decided here rather than in the module that owns it.
        let decides_default = code.contains(r#"unwrap_or("ATOMCODE"#)
            || code.contains(r#"unwrap_or_else(|| "ATOMCODE"#);
        reaches_env || decides_default
    });
    assert!(
        offenders.is_empty(),
        "resolution lives in `model_source`; these do it from somewhere else:\n{}",
        offenders.join("\n")
    );
}

/// And the same for the config file: one loader, so no row can resolve a
/// `[models.*]` selection by a rule of its own.
#[test]
fn only_one_module_loads_the_model_config() {
    let offenders =
        offenders_in_src(|code| code.contains("Config::load") || code.contains("resolve_model("));
    assert!(
        offenders.is_empty(),
        "loading the model config belongs in `model_source`, not here:\n{}",
        offenders.join("\n")
    );
}
