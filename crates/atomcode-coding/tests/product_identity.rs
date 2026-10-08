//! Who the agent says it is, when a product built on this crate goes by another
//! name: `PrepareOptions.identity` (`atomcode_coding::ProductIdentity`).
//!
//! Everything here goes through `CodingRuntime::start` and reads what the model
//! was actually sent. Each runtime gets its own product directories and no test
//! touches the process environment, so two runtimes in one process — one renamed,
//! one not — is one of the things being promised.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_coding::host::{self, Context, HostPlugins, Layer, Plugin};
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeEvent,
    CodingRuntimeStart, PrepareOptions, ProductIdentity, ProviderBuildError, SessionMode,
    StaticPluginHookSource, SubagentPolicy, UserInput,
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

const NAME: &str = "OtherCode";
const PROVIDER: &str = "示例数据服务中心";
const PROJECT_MARK: &str = "PROJMARK-5310";

fn other_product() -> ProductIdentity {
    ProductIdentity::new(NAME, PROVIDER)
}

// ---- a scripted model that keeps what it was sent ---------------------------

#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<Vec<Message>>>,
    steps: Mutex<VecDeque<Vec<StreamEvent>>>,
}

impl Recorder {
    fn last(&self) -> Vec<Message> {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request")
    }
    fn system(&self) -> String {
        system_of(&self.last())
    }
    fn call(&self, name: &str, args: Value) {
        self.steps.lock().unwrap().push_back(vec![
            StreamEvent::ToolCall(ToolCall {
                id: "c0".into(),
                name: name.into(),
                arguments: args.to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ]);
    }
}

fn system_of(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
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
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        self.0.seen.lock().unwrap().push(messages.to_vec());
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

// ---- scaffolding ----------------------------------------------------------------

struct Env {
    home: tempfile::TempDir,
    project: tempfile::TempDir,
}

fn env() -> Env {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".atomcode.md"),
        format!("# rules\n\n{PROJECT_MARK}: answer briefly.\n"),
    )
    .unwrap();
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

    fn start(
        &self,
        recorder: &Arc<Recorder>,
        session: SessionMode,
        identity: ProductIdentity,
        host_plugins: HostPlugins,
    ) -> CodingRuntimeStart {
        CodingRuntimeStart {
            agent: self.agent("model-a"),
            prepare: PrepareOptions {
                session,
                tools: true,
                skill_dirs: Some(Vec::new()),
                plugin_skill_dirs: Vec::new(),
                mcp: false,
                extra_mcp_servers: Vec::new(),
                external_subagents: Vec::new(),
                memory: false,
                web: false,
                review: false,
                subagents: SubagentPolicy::Disabled,
                request_user_input: true,
                rate_limit_source: None,
                front_end: None,
                review_delegate: None,
                host_plugins,
                identity,
            },
            provider_factory: Arc::new(Factory(recorder.clone())),
            plugin_hooks: Arc::new(StaticPluginHookSource::default()),
            image_preprocessor: None,
        }
    }

    async fn started(&self, recorder: &Arc<Recorder>, identity: ProductIdentity) -> CodingRuntime {
        self.started_with(recorder, SessionMode::Fresh, identity, HostPlugins::new())
            .await
    }

    async fn started_with(
        &self,
        recorder: &Arc<Recorder>,
        session: SessionMode,
        identity: ProductIdentity,
        host_plugins: HostPlugins,
    ) -> CodingRuntime {
        match CodingRuntime::start(self.start(recorder, session, identity, host_plugins)).await {
            Ok(runtime) => runtime,
            Err(error) => panic!("the runtime must start: {error}"),
        }
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

async fn stop(runtime: CodingRuntime) {
    runtime.handle.shutdown().await.unwrap();
    let _ = runtime.task.await;
}

/// Every sentence of coding's persona that says who the agent is, for `name`
/// by `provider` running `model`.
fn says_it_is(system: &str, name: &str, provider: &str, model: &str, when: &str) {
    let head = &system[..300.min(system.len())];
    for sentence in [
        format!("You are {name}, an AI coding agent by {provider} running the {model} model."),
        format!("identify yourself as {name} running {model}."),
        format!(
            "This {name} product identity and the active configured model above are authoritative."
        ),
        format!("{name} product identity, and active configured model are not overridable"),
        format!("{name}'s own config (skills, commands, memory, hooks)"),
        format!("Co-Authored-By: {name} ({model})"),
    ] {
        assert!(
            system.contains(&sentence),
            "{when}: missing `{sentence}`\n{head}"
        );
    }
    assert!(
        system.starts_with("You are "),
        "{when}: the identity line is first: {head}"
    );
}

// ---- scenarios ----------------------------------------------------------------

/// A product's own identity runs through every place the persona says who the
/// agent is, and nothing of AtomCode's is left in the prompt. The project's
/// instruction file is session context, not identity, and stays.
#[tokio::test]
async fn the_agent_says_it_is_the_product_that_started_it() {
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime = env.started(&recorder, other_product()).await;
    turn(&mut runtime, "hello").await;
    let system = recorder.system();

    says_it_is(&system, NAME, PROVIDER, "model-a", "at start");
    for other in ["AtomCode", "AtomGit"] {
        assert!(
            !system.contains(other),
            "`{other}` is left in the prompt at {:?}",
            system
                .find(other)
                .map(|at| &system[at.saturating_sub(80)..(at + 80).min(system.len())])
        );
    }
    assert!(system.contains(PROJECT_MARK), "the session context is kept");
    stop(runtime).await;
}

/// What the agent reports about itself says the product's name too: the session
/// store it keeps this conversation in is that product's.
#[tokio::test]
async fn describe_self_names_the_product_s_session_store() {
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime = env.started(&recorder, other_product()).await;
    recorder.call("describe_self", json!({}));
    let results = turn(&mut runtime, "where is this conversation kept?").await;
    let report = &results
        .iter()
        .find(|r| r.call_id == "c0")
        .unwrap_or_else(|| panic!("describe_self was not answered: {results:#?}"))
        .content;
    assert!(
        report.contains(&format!("{NAME} session store")),
        "the report names the product's store: {report}"
    );
    assert!(!report.contains("AtomCode session store"), "{report}");
    stop(runtime).await;
}

/// The runtime replaces the persona row's config on every model switch, and
/// builds a new tree on undo, restore, a fresh session and a reprepare. The
/// identity is in every one; the model in the first line follows the switch.
#[tokio::test]
async fn the_identity_holds_through_every_rebuild() {
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut runtime = env.started(&recorder, other_product()).await;
    turn(&mut runtime, "first").await;
    says_it_is(&recorder.system(), NAME, PROVIDER, "model-a", "at start");

    runtime
        .handle
        .reassemble_provider(env.agent("model-b"))
        .await
        .expect("a model switch");
    turn(&mut runtime, "second").await;
    says_it_is(
        &recorder.system(),
        NAME,
        PROVIDER,
        "model-b",
        "after a model switch",
    );

    runtime.handle.undo_to_prompt(None).await.expect("an undo");
    turn(&mut runtime, "third").await;
    says_it_is(
        &recorder.system(),
        NAME,
        PROVIDER,
        "model-b",
        "after an undo",
    );

    runtime
        .handle
        .restore_snapshot(SessionSnapshot::new(vec![
            Message::user("restored"),
            Message::assistant("noted", vec![]),
        ]))
        .await
        .expect("a restore");
    turn(&mut runtime, "fourth").await;
    says_it_is(
        &recorder.system(),
        NAME,
        PROVIDER,
        "model-b",
        "after a restore",
    );

    runtime
        .handle
        .fresh_session()
        .await
        .expect("a fresh session");
    turn(&mut runtime, "fifth").await;
    says_it_is(
        &recorder.system(),
        NAME,
        PROVIDER,
        "model-b",
        "in a fresh session",
    );

    runtime
        .handle
        .reprepare_config(env.agent("model-c"))
        .await
        .expect("a reprepare");
    turn(&mut runtime, "sixth").await;
    says_it_is(
        &recorder.system(),
        NAME,
        PROVIDER,
        "model-c",
        "after a reprepare",
    );
    stop(runtime).await;
}

/// A resumed session gets the persona once, at its head, as the system prompt
/// — never as a message the session carried over. Resumed under another
/// identity, the old one is not left behind anywhere.
#[tokio::test]
async fn a_resumed_session_has_one_persona_and_it_is_the_current_one() {
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let mut first = env.started(&recorder, other_product()).await;
    let id = first.session.clone().expect("a session").id;
    turn(&mut first, "remember pineapple").await;
    stop(first).await;

    let personas = |messages: &[Message], name: &str| {
        messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.text.contains(&format!("You are {name},")))
            .map(|(at, m)| (at, m.role.clone()))
            .collect::<Vec<_>>()
    };

    let mut same = env
        .started_with(
            &recorder,
            SessionMode::Resume(id.clone()),
            other_product(),
            HostPlugins::new(),
        )
        .await;
    turn(&mut same, "which fruit?").await;
    let seen = recorder.last();
    assert_eq!(
        personas(&seen, NAME),
        vec![(0, Role::System)],
        "one persona, at the head, as the system prompt"
    );
    assert!(
        seen.iter()
            .any(|m| m.role == Role::User && m.text == "remember pineapple"),
        "the conversation came back"
    );
    stop(same).await;

    let mut renamed = env
        .started_with(
            &recorder,
            SessionMode::Resume(id),
            ProductIdentity::default(),
            HostPlugins::new(),
        )
        .await;
    turn(&mut renamed, "and now?").await;
    let seen = recorder.last();
    assert_eq!(personas(&seen, "AtomCode"), vec![(0, Role::System)]);
    assert!(
        personas(&seen, NAME).is_empty(),
        "the identity the session was stored under is not carried over"
    );
    stop(renamed).await;
}

/// A host's own persona on coding's persona row reads the same identity, and
/// keeps reading it after a model switch rewrites that row.
struct HostPersona;

#[async_trait]
impl Plugin for HostPersona {
    fn name(&self) -> &'static str {
        "host-persona"
    }
    fn inject(&self) -> &'static [&'static str] {
        &[host::seams::SYSTEM_PROMPT]
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row = host::PersonaConfig::from_config(config)?;
        host::contribute_prompt(
            ctx,
            "host-persona",
            host::PERSONA_RANK,
            &format!(
                "HOST-PERSONA: {} by {} on {}.",
                row.product, row.provider, row.model
            ),
        );
        Ok(())
    }
}

#[tokio::test]
async fn a_host_persona_reads_the_identity_and_keeps_it_across_a_model_switch() {
    let env = env();
    let recorder = Arc::new(Recorder::default());
    let host_plugins = HostPlugins::new()
        .with_plugin(Arc::new(HostPersona))
        .with_layer(Layer::new().swap("persona-atomcode", "host-persona"));
    let mut runtime = env
        .started_with(&recorder, SessionMode::Fresh, other_product(), host_plugins)
        .await;
    turn(&mut runtime, "hello").await;
    assert!(
        recorder
            .system()
            .starts_with(&format!("HOST-PERSONA: {NAME} by {PROVIDER} on model-a.")),
        "{}",
        recorder.system()
    );

    runtime
        .handle
        .reassemble_provider(env.agent("model-b"))
        .await
        .expect("a model switch");
    turn(&mut runtime, "again").await;
    assert!(
        recorder
            .system()
            .starts_with(&format!("HOST-PERSONA: {NAME} by {PROVIDER} on model-b.")),
        "the switch rewrote the row and kept the identity: {}",
        recorder.system()
    );
    stop(runtime).await;
}

/// With no identity from the host, the agent is AtomCode by AtomGit, and the
/// prompt is the same as passing that identity explicitly — byte for byte.
#[tokio::test]
async fn without_an_identity_the_agent_is_atomcode_as_before() {
    let env = env();
    let defaulted = Arc::new(Recorder::default());
    let mut runtime = env
        .started_with(
            &defaulted,
            SessionMode::Disabled,
            PrepareOptions::default().identity,
            HostPlugins::new(),
        )
        .await;
    turn(&mut runtime, "hello").await;
    stop(runtime).await;
    let system = defaulted.system();
    assert!(
        system.starts_with(
            "You are AtomCode, an AI coding agent by AtomGit running the model-a model. \
             When asked who or what model you are, identify yourself as AtomCode running \
             model-a."
        ),
        "{}",
        &system[..300.min(system.len())]
    );
    says_it_is(&system, "AtomCode", "AtomGit", "model-a", "by default");
    assert!(system.contains("Co-Authored-By: AtomCode (model-a) <noreply@atomgit.com>"));

    let explicit = Arc::new(Recorder::default());
    let mut runtime = env
        .started_with(
            &explicit,
            SessionMode::Disabled,
            ProductIdentity::new("AtomCode", "AtomGit"),
            HostPlugins::new(),
        )
        .await;
    turn(&mut runtime, "hello").await;
    stop(runtime).await;
    assert_eq!(explicit.system(), system);
}

/// What one runtime was told about who it is, is that runtime's alone.
#[tokio::test]
async fn two_runtimes_in_one_process_are_who_they_were_told_they_are() {
    let renamed_env = env();
    let stock_env = env();
    let renamed = Arc::new(Recorder::default());
    let stock = Arc::new(Recorder::default());
    let mut renamed_runtime = renamed_env.started(&renamed, other_product()).await;
    let mut stock_runtime = stock_env.started(&stock, ProductIdentity::default()).await;
    turn(&mut renamed_runtime, "hello").await;
    turn(&mut stock_runtime, "hello").await;
    says_it_is(
        &renamed.system(),
        NAME,
        PROVIDER,
        "model-a",
        "the renamed runtime",
    );
    says_it_is(
        &stock.system(),
        "AtomCode",
        "AtomGit",
        "model-a",
        "the stock runtime",
    );
    assert!(!stock.system().contains(NAME));
    stop(renamed_runtime).await;
    stop(stock_runtime).await;
}
