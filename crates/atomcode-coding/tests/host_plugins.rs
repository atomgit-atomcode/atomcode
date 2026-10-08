//! A host outside this workspace brings rows of its own to `CodingRuntime`.
//!
//! The door is `PrepareOptions.host_plugins` (`atomcode_coding::host`): plugins
//! the host wrote, and layers applied after everything the product and the
//! person said. Everything here goes through `CodingRuntime::start`, the entry
//! an embedding product calls, and reads what the model was actually sent.
//!
//! Each runtime gets its own product directories and no test touches the
//! process environment: two runtimes in one process, each with its own rows, is
//! one of the things being promised.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_coding::host::{self, Context, HostPlugins, Layer, Plugin};
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeEvent,
    CodingRuntimeStart, PrepareOptions, ProviderBootstrap, ProviderBuildError,
    ProviderUnavailableReason, SessionMode, StaticPluginHookSource, SubagentPolicy, UserInput,
};
use atomcode_kernel::event::AgentEvent;
use atomcode_kernel::message::{Message, Role, SessionSnapshot};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::{ToolCall, ToolDef, ToolResult};
use serde_json::{json, Value};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

const PERSONA_MARK: &str = "HOST-PERSONA-4417";
const PROJECT_MARK: &str = "PROJMARK-7731";

/// The code-analysis rows and what each puts in front of the model.
const CODE_ROWS: &[(&str, &[&str], &str)] = &[
    (
        "codeintel",
        &["list_symbols", "read_symbol", "find_references"],
        "Use `list_symbols` / `read_symbol` to read code by symbol",
    ),
    (
        "code-graph",
        &[
            "trace_callers",
            "trace_callees",
            "trace_chain",
            "blast_radius",
            "file_dependencies",
        ],
        "Before changing a shared function, use `trace_callers`",
    ),
    (
        "tool-ast-grep",
        &["ast_grep"],
        "Use `ast_grep` when you are looking for a code *shape*",
    ),
];

// ---- a scripted model that keeps what it was sent ---------------------------

#[derive(Clone, Debug)]
struct Seen {
    system: String,
    tools: Vec<ToolDef>,
}

impl Seen {
    fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name.clone()).collect()
    }
    fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }
}

#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<Seen>>,
    steps: Mutex<VecDeque<Vec<StreamEvent>>>,
}

impl Recorder {
    fn last(&self) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request")
    }
    /// The next answer asks for these calls in one round.
    fn call(&self, calls: &[(&str, Value)]) {
        let mut events: Vec<StreamEvent> = calls
            .iter()
            .enumerate()
            .map(|(n, (name, args))| {
                StreamEvent::ToolCall(ToolCall {
                    id: format!("c{n}"),
                    name: (*name).into(),
                    arguments: args.to_string(),
                })
            })
            .collect();
        events.push(StreamEvent::Done { truncated: false });
        self.steps.lock().unwrap().push_back(events);
    }
}

struct Model(Arc<Recorder>, String);

#[async_trait]
impl LlmProvider for Model {
    fn model_name(&self) -> &str {
        &self.1
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        let system = messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        self.0.seen.lock().unwrap().push(Seen {
            system,
            tools: tools.to_vec(),
        });
        let events = self.0.steps.lock().unwrap().pop_front().unwrap_or_else(|| {
            vec![
                StreamEvent::TextDelta("done".into()),
                StreamEvent::Done { truncated: false },
            ]
        });
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct Factory(Arc<Recorder>);

impl CodingProviderFactory for Factory {
    fn build(
        &self,
        config: &CodingAgentConfig,
        _: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        Ok(Arc::new(Model(self.0.clone(), config.model.clone())))
    }
}

// ---- the host's own persona ---------------------------------------------------

/// A persona written outside this crate, swapped onto `persona-atomcode`.
struct HostPersona {
    applies: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for HostPersona {
    fn name(&self) -> &'static str {
        "host-persona"
    }
    fn inject(&self) -> &'static [&'static str] {
        &[host::seams::SYSTEM_PROMPT]
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        let row = host::PersonaConfig::from_config(config)?;
        host::contribute_prompt(
            ctx,
            "host-persona",
            host::PERSONA_RANK,
            &format!(
                "{PERSONA_MARK}: a host's own assistant running {}.",
                row.model
            ),
        );
        Ok(())
    }
}

/// Answers to a name that is already taken.
struct NamedAs(&'static str);

#[async_trait]
impl Plugin for NamedAs {
    fn name(&self) -> &'static str {
        self.0
    }
    async fn apply(&self, _: &Context, _: &Value) -> Result<(), String> {
        Ok(())
    }
}

fn host_persona(applies: &Arc<AtomicUsize>) -> HostPlugins {
    HostPlugins::new()
        .with_plugin(Arc::new(HostPersona {
            applies: applies.clone(),
        }))
        .with_layer(Layer::new().swap("persona-atomcode", "host-persona"))
}

fn excluding(patterns: &[&str]) -> Layer {
    Layer::new()
        .patch("tools", json!({ "exclude": patterns }))
        .expect("a tools patch")
}

// ---- scaffolding ----------------------------------------------------------------

struct Env {
    home: tempfile::TempDir,
    project: tempfile::TempDir,
}

/// A project with an instruction file, and product directories of its own.
fn env() -> Env {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".atomcode.md"),
        format!("# rules\n\n{PROJECT_MARK}: answer in Simplified Chinese.\n"),
    )
    .unwrap();
    std::fs::write(project.path().join("README.md"), "# a project\n").unwrap();
    Env {
        home: tempfile::tempdir().unwrap(),
        project,
    }
}

impl Env {
    fn agent(&self, model: &str) -> CodingAgentConfig {
        let dirs = atomcode_capabilities::ProductDirs::new(
            self.home.path(),
            atomcode_config::distribution::PROJECT_DIR_NAME,
        )
        .with_home_dir_name(atomcode_config::distribution::HOME_DIR_NAME);
        CodingAgentConfig::new(
            "key",
            "https://example.test/v1",
            model,
            self.project.path(),
            dirs,
        )
    }

    fn start(&self, recorder: &Arc<Recorder>, host: HostPlugins) -> CodingRuntimeStart {
        CodingRuntimeStart {
            agent: self.agent("model-a"),
            prepare: PrepareOptions {
                session: SessionMode::Fresh,
                tools: true,
                skill_dirs: Some(Vec::new()),
                plugin_skill_dirs: Vec::new(),
                mcp: false,
                extra_mcp_servers: Vec::new(),
                mcp_user_config: None,
                mcp_tokens: None,
                external_subagents: Vec::new(),
                memory: false,
                web: false,
                review: false,
                subagents: SubagentPolicy::Disabled,
                request_user_input: true,
                rate_limit_source: None,
                front_end: None,
                review_delegate: None,
                host_plugins: host,
                identity: Default::default(),
            },
            provider_factory: Arc::new(Factory(recorder.clone())),
            plugin_hooks: Arc::new(StaticPluginHookSource::default()),
            image_preprocessor: None,
        }
    }
}

async fn started(env: &Env, recorder: &Arc<Recorder>, host: HostPlugins) -> CodingRuntime {
    match CodingRuntime::start(env.start(recorder, host)).await {
        Ok(runtime) => runtime,
        Err(error) => panic!("the runtime must start: {error}"),
    }
}

/// The error a start fails with, or a panic if it does not fail.
async fn refused(env: &Env, host: HostPlugins, bootstrap: ProviderBootstrap) -> String {
    let recorder = Arc::new(Recorder::default());
    match CodingRuntime::start_with_bootstrap(env.start(&recorder, host), bootstrap).await {
        Ok(runtime) => {
            let _ = runtime.handle.shutdown().await;
            panic!("the start must be refused")
        }
        Err(error) => error.to_string(),
    }
}

/// Run a turn, allowing whatever is asked; the tool results it produced.
async fn turn(runtime: &mut CodingRuntime, text: &str) -> Vec<ToolResult> {
    runtime.handle.submit(UserInput::from(text)).await.unwrap();
    let mut results = Vec::new();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(30), runtime.events.recv())
            .await
            .unwrap_or_else(|_| panic!("`{text}`: the turn never finished"))
            .expect("the runtime's event stream closed");
        match event.event {
            CodingRuntimeEvent::TurnFinished(_) => return results,
            CodingRuntimeEvent::Request(request) => {
                runtime
                    .handle
                    .respond(request.id, json!({ "decision": "allow" }))
                    .await
                    .unwrap();
            }
            CodingRuntimeEvent::Agent(AgentEvent::ToolResult { result }) => results.push(result),
            _ => {}
        }
    }
}

/// What the product sends with nothing from a host — the control every
/// scenario below is measured against.
async fn product_request(env: &Env) -> Seen {
    let recorder = Arc::new(Recorder::default());
    let mut runtime = started(env, &recorder, HostPlugins::new()).await;
    turn(&mut runtime, "hello").await;
    runtime.handle.shutdown().await.unwrap();
    recorder.last()
}

/// The first sentence of coding's own persona for `model`: its identity line,
/// however the product is branded.
fn product_identity(env: &Env, model: &str) -> String {
    let persona = atomcode_coding::coding_persona(model, true, true, &env.agent(model).dirs);
    let end = persona.find(". ").expect("an identity sentence") + 1;
    persona[..end].to_string()
}

// ---- scenarios ----------------------------------------------------------------

/// The host's persona is the identity line; coding's is gone, and the session
/// context (the project's instruction file) is still there.
#[tokio::test]
async fn a_host_persona_takes_the_place_of_codings() {
    let env = env();
    let identity = product_identity(&env, "model-a");
    let product = product_request(&env).await;
    assert!(
        product.system.starts_with(&identity),
        "the control: coding's persona opens the product's prompt: {}",
        &product.system[..200.min(product.system.len())]
    );

    let recorder = Arc::new(Recorder::default());
    let applies = Arc::new(AtomicUsize::new(0));
    let mut runtime = started(&env, &recorder, host_persona(&applies)).await;
    turn(&mut runtime, "hello").await;
    let seen = recorder.last();
    assert!(
        seen.system.starts_with(&format!(
            "{PERSONA_MARK}: a host's own assistant running model-a."
        )),
        "the host's persona is the identity line, naming the model: {}",
        &seen.system[..200.min(seen.system.len())]
    );
    assert!(
        !seen.system.contains(&identity),
        "coding's identity line must be gone"
    );
    assert!(
        seen.system.contains(PROJECT_MARK),
        "the project's instruction file is session context, not persona, and stays"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// Excluded tools are not offered, and a model that calls one anyway does not
/// reach it.
#[tokio::test]
async fn a_host_layer_takes_tools_off_the_model_and_out_of_reach() {
    let env = env();
    let product = product_request(&env).await;
    for name in [
        "ast_grep",
        "trace_callers",
        "trace_chain",
        "schedule_wakeup",
    ] {
        assert!(product.has_tool(name), "the control offers `{name}`");
    }

    let recorder = Arc::new(Recorder::default());
    let host =
        HostPlugins::new().with_layer(excluding(&["ast_grep", "trace_*", "schedule_wakeup"]));
    let mut runtime = started(&env, &recorder, host).await;
    recorder.call(&[
        ("ast_grep", json!({ "pattern": "fn $NAME($$$) { $$$ }" })),
        ("schedule_wakeup", json!({})),
        (
            "read_file",
            json!({ "file_path": env.project.path().join("README.md") }),
        ),
    ]);
    let results = turn(&mut runtime, "look around").await;

    let seen = recorder.seen.lock().unwrap()[0].clone();
    for name in [
        "ast_grep",
        "trace_callers",
        "trace_callees",
        "trace_chain",
        "schedule_wakeup",
    ] {
        assert!(
            !seen.has_tool(name),
            "`{name}` is still offered: {:?}",
            seen.names()
        );
    }
    assert!(
        seen.has_tool("blast_radius") && seen.has_tool("read_file"),
        "only what was excluded goes: {:?}",
        seen.names()
    );

    let result = |id: &str| {
        results
            .iter()
            .find(|r| r.call_id == id)
            .unwrap_or_else(|| panic!("no result for `{id}`: {results:#?}"))
    };
    for (id, name) in [("c0", "ast_grep"), ("c1", "schedule_wakeup")] {
        let result = result(id);
        assert!(
            result.is_error && result.content.contains(name),
            "a call to the excluded `{name}` must fail as unknown: {result:?}"
        );
    }
    let control = result("c2");
    assert!(
        !control.is_error && control.content.contains("a project"),
        "the control in the same round runs: {control:?}"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// A disabled row takes its tools and its prompt fragment with it.
#[tokio::test]
async fn a_host_layer_switches_off_whole_rows() {
    let env = env();
    let product = product_request(&env).await;
    for (row, tools, fragment) in CODE_ROWS {
        for tool in *tools {
            assert!(
                product.has_tool(tool),
                "the control: `{row}` offers `{tool}`"
            );
        }
        assert!(
            product.system.contains(fragment),
            "the control: `{row}` says `{fragment}`"
        );
    }

    let recorder = Arc::new(Recorder::default());
    let mut off = Layer::new();
    for (row, _, _) in CODE_ROWS {
        off = off.disable(*row);
    }
    let mut runtime = started(&env, &recorder, HostPlugins::new().with_layer(off)).await;
    turn(&mut runtime, "hello").await;
    let seen = recorder.last();
    for (row, tools, fragment) in CODE_ROWS {
        for tool in *tools {
            assert!(
                !seen.has_tool(tool),
                "`{row}` is off, `{tool}` must be too: {:?}",
                seen.names()
            );
        }
        assert!(
            !seen.system.contains(fragment),
            "`{row}` is off, its fragment must be too"
        );
    }
    assert_eq!(
        seen.tools.len(),
        product.tools.len() - 9,
        "nine tools go with the three rows, and nothing else"
    );
    runtime.handle.shutdown().await.unwrap();
}

/// The runtime replaces its tree for reasons of its own — a model switch, a
/// restored snapshot, a fresh session, a reprepare — and changes the
/// conversation under it on an undo. What the host brought must hold through
/// every one.
///
/// Where an operation does remount, the persona's `apply` count says so: the
/// rows are checked in a tree that was really built again, not in the one the
/// host started with.
#[tokio::test]
async fn what_the_host_brought_holds_through_every_rebuild() {
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let applies = Arc::new(AtomicUsize::new(0));
    let host = host_persona(&applies).with_layer(excluding(&["schedule_wakeup"]));
    let mut runtime = started(&env, &recorder, host).await;

    let holds = |recorder: &Recorder, model: &str, when: &str| {
        let seen = recorder.last();
        assert!(
            seen.system.starts_with(&format!(
                "{PERSONA_MARK}: a host's own assistant running {model}."
            )),
            "{when}: the host's persona is gone or stale: {}",
            &seen.system[..200.min(seen.system.len())]
        );
        assert!(
            !seen.has_tool("schedule_wakeup"),
            "{when}: the excluded tool is back"
        );
        assert!(seen.system.contains(PROJECT_MARK), "{when}: context lost");
    };
    let remounted = |before: usize, when: &str| {
        assert!(
            applies.load(Ordering::SeqCst) > before,
            "{when}: the persona row was not mounted again, so this checked the old tree"
        );
    };

    turn(&mut runtime, "first").await;
    holds(&recorder, "model-a", "at start");

    // A patch on the live tree: the persona row's config changes, so it remounts.
    let before = applies.load(Ordering::SeqCst);
    runtime
        .handle
        .reassemble_provider(env.agent("model-b"))
        .await
        .expect("a model switch");
    turn(&mut runtime, "second").await;
    holds(&recorder, "model-b", "after a model switch");
    remounted(before, "a model switch");

    // On a live session an undo is a fact appended to the log, and the tree
    // stays (`docs/adr/0024`); it is here for the conversation, not the mount.
    runtime.handle.undo_to_prompt(None).await.expect("an undo");
    turn(&mut runtime, "third").await;
    holds(&recorder, "model-b", "after an undo");

    let before = applies.load(Ordering::SeqCst);
    runtime
        .handle
        .restore_snapshot(SessionSnapshot::new(vec![
            Message::user("restored"),
            Message::assistant("noted", vec![]),
        ]))
        .await
        .expect("a restore");
    turn(&mut runtime, "fourth").await;
    holds(&recorder, "model-b", "after a restored snapshot");
    remounted(before, "a restored snapshot");

    let before = applies.load(Ordering::SeqCst);
    runtime
        .handle
        .fresh_session()
        .await
        .expect("a fresh session");
    turn(&mut runtime, "fifth").await;
    holds(&recorder, "model-b", "in a fresh session");
    remounted(before, "a fresh session");

    // `reload_capabilities` would not do here: with no MCP server and nothing in
    // flight it reloads skills into the live tree rather than rebuilding it.
    let before = applies.load(Ordering::SeqCst);
    runtime
        .handle
        .reprepare_config(env.agent("model-b"))
        .await
        .expect("a reprepare");
    turn(&mut runtime, "sixth").await;
    holds(&recorder, "model-b", "after a reprepare");
    remounted(before, "a reprepare");

    runtime.handle.shutdown().await.unwrap();
}

/// What one runtime's host brought is that runtime's alone.
#[tokio::test]
async fn two_runtimes_in_one_process_keep_their_own_rows() {
    let custom_env = env();
    let stock_env = env();
    let custom = Arc::new(Recorder::default());
    let stock = Arc::new(Recorder::default());
    let applies = Arc::new(AtomicUsize::new(0));
    let mut custom_runtime = started(
        &custom_env,
        &custom,
        host_persona(&applies).with_layer(excluding(&["schedule_wakeup"])),
    )
    .await;
    let mut stock_runtime = started(&stock_env, &stock, HostPlugins::new()).await;

    turn(&mut custom_runtime, "hello").await;
    turn(&mut stock_runtime, "hello").await;

    let (custom, stock) = (custom.last(), stock.last());
    assert!(custom.system.starts_with(PERSONA_MARK) && !custom.has_tool("schedule_wakeup"));
    assert!(
        stock
            .system
            .starts_with(&product_identity(&stock_env, "model-a"))
            && !stock.system.contains(PERSONA_MARK)
            && stock.has_tool("schedule_wakeup"),
        "the other runtime is the product, untouched"
    );
    custom_runtime.handle.shutdown().await.unwrap();
    stock_runtime.handle.shutdown().await.unwrap();
}

/// Rows that cannot mount fail the start, saying which name is wrong — with a
/// provider, and without one (a runtime started to wait for a login).
#[tokio::test]
async fn rows_that_cannot_mount_fail_the_start_by_name() {
    let env = env();
    let cases: Vec<(HostPlugins, &[&str])> = vec![
        // One of coding's own row names.
        (
            HostPlugins::new().with_plugin(Arc::new(NamedAs("persona-atomcode"))),
            &["persona-atomcode", "already"],
        ),
        // One of the harness catalog's.
        (
            HostPlugins::new().with_plugin(Arc::new(NamedAs("tool-bash"))),
            &["tool-bash", "already"],
        ),
        // The host's own, twice.
        (
            HostPlugins::new()
                .with_plugin(Arc::new(NamedAs("mine")))
                .with_plugin(Arc::new(NamedAs("mine"))),
            &["mine", "twice"],
        ),
        // A row pointed at a plugin nobody registered.
        (
            HostPlugins::new().with_layer(Layer::new().swap("persona-atomcode", "nobody")),
            &["persona-atomcode", "nobody"],
        ),
        // A patch aimed at a row nobody inserted.
        (
            HostPlugins::new().with_layer(Layer::new().disable("no-such-row")),
            &["no-such-row"],
        ),
    ];
    for (host, names) in cases {
        for bootstrap in [
            ProviderBootstrap::Required,
            ProviderBootstrap::Unavailable(ProviderUnavailableReason::NotConfigured),
        ] {
            let label = format!("{host:?} / {bootstrap:?}");
            let error = refused(&env, host.clone(), bootstrap).await;
            for name in names {
                assert!(
                    error.contains(name),
                    "{label}: the error must say `{name}`: {error}"
                );
            }
        }
    }
}

/// With nothing from the host, the model is sent exactly what the product
/// sends: same prompt, same tools, byte for byte — the prompt cache depends
/// on it.
#[tokio::test]
async fn with_nothing_from_the_host_the_model_is_sent_exactly_the_product() {
    let env = env();
    let product = product_request(&env).await;

    let recorder = Arc::new(Recorder::default());
    let mut runtime = started(&env, &recorder, HostPlugins::new().with_layer(Layer::new())).await;
    turn(&mut runtime, "hello").await;
    let seen = recorder.last();
    runtime.handle.shutdown().await.unwrap();

    assert_eq!(seen.system, product.system);
    let wire = |tools: &[ToolDef]| {
        tools
            .iter()
            .map(|t| format!("{}\u{1}{}\u{1}{}", t.name, t.description, t.parameters))
            .collect::<Vec<_>>()
    };
    assert_eq!(wire(&seen.tools), wire(&product.tools));
}

/// What `host::PUBLISHED_ROWS` promises, against the tree the runtime mounts:
/// every published row is in it, the persona row's config reads as
/// `PersonaConfig`, and the `tools` row's config is left to the host.
#[tokio::test]
async fn the_published_rows_are_what_the_product_mounts() {
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let parts = atomcode_coding::prepare(
        &env.agent("model-a"),
        env.start(&recorder, HostPlugins::new()).prepare,
    )
    .await
    .expect("prepare");
    let cfg = env.agent("model-a");
    let mounted = atomcode_coding::runtime::mount(
        &parts,
        &cfg,
        &env.start(&recorder, HostPlugins::new()).prepare,
        Arc::new(Model(recorder.clone(), "model-a".into())),
    )
    .await
    .expect("the product mounts");
    let rows: Vec<(String, Value)> = mounted
        .app
        .tree()
        .active()
        .map(|e| (e.id.clone(), e.config.clone()))
        .collect();
    for published in host::PUBLISHED_ROWS {
        assert!(
            rows.iter().any(|(id, _)| id == published),
            "`{published}` is published and must be mounted: {:?}",
            rows.iter().map(|(id, _)| id).collect::<Vec<_>>()
        );
    }
    let config = |id: &str| rows.iter().find(|(r, _)| r == id).unwrap().1.clone();
    assert_eq!(
        host::PersonaConfig::from_config(&config("persona-atomcode"))
            .expect("the persona row's config is a PersonaConfig")
            .model,
        "model-a"
    );
    let tools = config("tools");
    assert!(
        tools.is_null() || tools.as_object().is_some_and(|o| o.is_empty()),
        "the runtime writes no policy into the `tools` row, so a host's patch is the whole \
         of it: {tools}"
    );
}
