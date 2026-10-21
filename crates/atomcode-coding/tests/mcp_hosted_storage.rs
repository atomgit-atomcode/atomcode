//! A host that keeps the user-level MCP config and the MCP OAuth tokens itself —
//! encrypted, say — hands them to the runtime as `DocumentStore`s
//! (`PrepareOptions::mcp_user_config`, `PrepareOptions::mcp_tokens`).
//!
//! Everything here goes through `CodingRuntime::start`. The user tree holds a
//! `mcp.json` that is not JSON at all: a runtime that read it would fail its MCP
//! config, and one that wrote it would change it — so every scenario also checks
//! it was left alone.
#![cfg(unix)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_coding::host::{DocumentResult, DocumentStore};
use atomcode_coding::parts::McpAction;
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeEvent,
    CodingRuntimeStart, PrepareOptions, ProviderBuildError, SessionMode, StaticPluginHookSource,
    SubagentPolicy, UserInput,
};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::ToolDef;
use serde_json::{json, Value};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

// ---- a document kept sealed ------------------------------------------------------

/// Stored with a prefix the runtime must never see or write; `update` holds the
/// lock across read, edit and write, as a host's store holds its own.
#[derive(Debug, Default)]
struct Sealed {
    stored: Mutex<Option<String>>,
    updates: Mutex<usize>,
}

const SEAL: &str = "SEALED:";

impl Sealed {
    fn holding(plain: &str) -> Arc<Self> {
        let doc = Self::default();
        *doc.stored.lock().unwrap() = Some(format!("{SEAL}{plain}"));
        Arc::new(doc)
    }
    fn raw(&self) -> Option<String> {
        self.stored.lock().unwrap().clone()
    }
    fn plain(&self) -> String {
        self.raw()
            .and_then(|raw| raw.strip_prefix(SEAL).map(str::to_string))
            .expect("stored sealed")
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.plain()).expect("a JSON config")
    }
    fn open(raw: &Option<String>) -> DocumentResult<Option<String>> {
        match raw {
            None => Ok(None),
            Some(raw) => match raw.strip_prefix(SEAL) {
                Some(plain) => Ok(Some(plain.to_string())),
                None => Err(std::io::Error::other("not sealed with this key").into()),
            },
        }
    }
}

impl DocumentStore for Sealed {
    fn read(&self) -> DocumentResult<Option<String>> {
        Self::open(&self.stored.lock().unwrap())
    }
    fn update(
        &self,
        edit: &mut dyn FnMut(Option<&str>) -> DocumentResult<Option<String>>,
    ) -> DocumentResult<()> {
        let mut stored = self.stored.lock().unwrap();
        let plain = Self::open(&stored)?;
        let next = edit(plain.as_deref())?;
        *stored = next.map(|text| format!("{SEAL}{text}"));
        *self.updates.lock().unwrap() += 1;
        Ok(())
    }
}

// ---- a scripted model that keeps the tools it was offered ---------------------------

#[derive(Default)]
struct Recorder {
    offered: Mutex<Vec<Vec<String>>>,
    steps: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<(Vec<ToolDef>, Vec<Message>)>>,
}

impl Recorder {
    fn last_offered(&self) -> Vec<String> {
        self.offered
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

struct Model(Arc<Recorder>);

#[async_trait]
impl LlmProvider for Model {
    fn model_name(&self) -> &str {
        "model-a"
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        self.0
            .requests
            .lock()
            .unwrap()
            .push((tools.to_vec(), messages.to_vec()));
        self.0
            .offered
            .lock()
            .unwrap()
            .push(tools.iter().map(|t| t.name.clone()).collect());
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
        _: &CodingAgentConfig,
        _: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        Ok(Arc::new(Model(self.0.clone())))
    }
}

// ---- a minimal MCP server over stdio --------------------------------------------------

/// One `echo` tool; each start is a line in `spawns`.
fn write_mcp_server(dir: &std::path::Path, spawns: &std::path::Path) -> std::path::PathBuf {
    write_mcp_server_with_tools(
        dir,
        spawns,
        json!([{
            "name":"echo", "description":"echo back",
            "inputSchema":{"type":"object","properties":{}}
        }]),
    )
}

fn write_mcp_server_with_tools(
    dir: &std::path::Path,
    spawns: &std::path::Path,
    tools: Value,
) -> std::path::PathBuf {
    let tools_file = dir.join("tools.json");
    std::fs::write(&tools_file, tools.to_string()).unwrap();
    let script = dir.join("server.sh");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
echo started >> "{spawns}"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"u","version":"0"}}}}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":' "$id"
      cat "{tools_file}"
      printf '}}}}\n' ;;
    *'"method":"tools/call"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"echo:from-server"}}]}}}}\n' "$id" ;;
    *'"id":'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
  esac
done
"#,
            spawns = spawns.display(),
            tools_file = tools_file.display()
        ),
    )
    .unwrap();
    script
}

// ---- scaffolding ----------------------------------------------------------------------

const UNREADABLE_FILE: &str = "not json at all";

struct Env {
    home: tempfile::TempDir,
    project: tempfile::TempDir,
    scratch: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            project: tempfile::tempdir().unwrap(),
            scratch: tempfile::tempdir().unwrap(),
        };
        std::fs::write(env.home.path().join("mcp.json"), UNREADABLE_FILE).unwrap();
        std::fs::write(
            env.project.path().join(".mcp.json"),
            r#"{ "mcpServers": { "p": { "command": "never-run", "disabled": true } } }"#,
        )
        .unwrap();
        env
    }

    fn spawns(&self) -> std::path::PathBuf {
        self.scratch.path().join("spawns.log")
    }

    /// The user-level config the host keeps: the stdio server `u`, trusted, and
    /// the OAuth server `h`, switched off so nothing dials it.
    fn user_config(&self) -> String {
        let script = write_mcp_server(self.scratch.path(), &self.spawns());
        json!({ "mcpServers": {
            "u": { "command": "sh", "args": [script], "trust": true, "env": { "TOKEN": "s3cret" } },
            "h": {
                "url": "http://127.0.0.1:9/mcp",
                "auth": { "type": "oauth", "provider": "x" },
                "disabled": true
            }
        }})
        .to_string()
    }

    fn agent(&self) -> CodingAgentConfig {
        let dirs = atomcode_capabilities::ProductDirs::new(
            self.home.path(),
            atomcode_config::distribution::PROJECT_DIR_NAME,
        )
        .with_home_dir_name(atomcode_config::distribution::HOME_DIR_NAME);
        let mut config = CodingAgentConfig::new(
            "key",
            "https://example.test/v1",
            "model-a",
            self.project.path(),
            dirs,
        );
        // Storage/lifecycle assertions observe publication directly through eager schemas.
        // The discovery test below explicitly uses the product default instead.
        config.mcp_tool_search = atomcode_config::config::McpToolSearchMode::Off;
        config
    }

    fn start(
        &self,
        recorder: &Arc<Recorder>,
        user_config: &Arc<Sealed>,
        tokens: &Arc<Sealed>,
    ) -> CodingRuntimeStart {
        CodingRuntimeStart {
            agent: self.agent(),
            prepare: PrepareOptions {
                session: SessionMode::Disabled,
                tools: true,
                skill_dirs: Some(Vec::new()),
                plugin_skill_dirs: Vec::new(),
                mcp: true,
                extra_mcp_servers: Vec::new(),
                mcp_user_config: Some(user_config.clone()),
                mcp_tokens: Some(tokens.clone()),
                external_subagents: Vec::new(),
                memory: false,
                web: false,
                review: false,
                subagents: SubagentPolicy::Disabled,
                request_user_input: true,
                rate_limit_source: None,
                front_end: None,
                review_delegate: None,
                host_plugins: Default::default(),
                identity: Default::default(),
            },
            provider_factory: Arc::new(Factory(recorder.clone())),
            plugin_hooks: Arc::new(StaticPluginHookSource::default()),
            image_preprocessor: None,
        }
    }

    fn user_file_untouched(&self) {
        assert_eq!(
            std::fs::read_to_string(self.home.path().join("mcp.json")).unwrap(),
            UNREADABLE_FILE,
            "the user tree's mcp.json is not the config: never read, never written"
        );
        assert!(
            !self.home.path().join("mcp_auth.toml").exists(),
            "no token was written to the user tree"
        );
    }
}

async fn started(start: CodingRuntimeStart) -> CodingRuntime {
    match CodingRuntime::start(start).await {
        Ok(runtime) => runtime,
        Err(error) => panic!("the runtime must start: {error}"),
    }
}

async fn turn(runtime: &mut CodingRuntime, text: &str) {
    runtime.handle.submit(UserInput::from(text)).await.unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(30), runtime.events.recv())
            .await
            .unwrap_or_else(|_| panic!("`{text}`: the turn never finished"))
            .expect("the runtime's event stream closed");
        match event.event {
            CodingRuntimeEvent::TurnFinished(_) => return,
            CodingRuntimeEvent::Request(request) => {
                runtime
                    .handle
                    .respond(request.id, json!({ "decision": "allow" }))
                    .await
                    .unwrap();
            }
            _ => {}
        }
    }
}

fn token_doc(server: &str) -> String {
    format!("[servers.{server}]\nprovider = \"x\"\ntoken_type = \"Bearer\"\naccess_token = \"t\"\n")
}

// ---- scenarios ----------------------------------------------------------------------

/// The runtime finds its user-level servers in the host's document — at start
/// and again after a rebuild — and the project's `.mcp.json` in its file.
#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_reads_the_host_kept_config_at_start_and_after_a_rebuild() {
    let env = Env::new();
    let recorder = Arc::new(Recorder::default());
    let user_config = Sealed::holding(&env.user_config());
    let tokens = Sealed::holding(&token_doc("h"));
    let mut runtime = started(env.start(&recorder, &user_config, &tokens)).await;
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();

    turn(&mut runtime, "hello").await;
    assert!(
        recorder.last_offered().iter().any(|t| t == "mcp__u__echo"),
        "the host-kept server's tool is offered: {:?}",
        recorder.last_offered()
    );

    let rows = runtime.handle.mcp_rows().await.unwrap().rows;
    let row = |name: &str| {
        rows.iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("no row for `{name}`: {rows:#?}"))
    };
    assert_eq!(
        row("u").config_path,
        None,
        "no file to show for a host-kept config"
    );
    assert_eq!(
        row("p").config_path,
        Some(env.project.path().join(".mcp.json")),
        "the project's config is its file"
    );
    assert!(row("p").disabled);
    assert!(
        row("h").oauth && row("h").authenticated,
        "the token is read from the host's token document"
    );

    runtime
        .handle
        .reprepare_config(env.agent())
        .await
        .expect("a rebuild");
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    turn(&mut runtime, "again").await;
    assert!(
        recorder.last_offered().iter().any(|t| t == "mcp__u__echo"),
        "the rebuilt tree reads the host's document too: {:?}",
        recorder.last_offered()
    );
    assert_eq!(
        std::fs::read_to_string(env.spawns())
            .unwrap()
            .lines()
            .count(),
        2,
        "connected at start and again after the rebuild"
    );
    assert_eq!(
        *user_config.updates.lock().unwrap(),
        0,
        "reading is not editing"
    );
    env.user_file_untouched();
    runtime.handle.shutdown().await.unwrap();
}

/// What the runtime writes — an "always allow", switching a server off and on,
/// signing out — goes into the host's documents through `update`, keeps what
/// was there, and is stored as the host keeps it.
#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_edits_the_host_kept_documents_through_update() {
    let env = Env::new();
    let recorder = Arc::new(Recorder::default());
    let user_config = Sealed::holding(&env.user_config());
    let tokens = Sealed::holding(&format!("{}\n{}", token_doc("h"), token_doc("other")));
    let runtime = started(env.start(&recorder, &user_config, &tokens)).await;
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();

    let approval = runtime
        .handle
        .approve_mcp_tool("mcp__u__echo".into())
        .await
        .unwrap()
        .expect("an MCP tool");
    assert_eq!(approval.persist_error, None, "kept: {approval:?}");
    assert_eq!(
        user_config.json()["mcpServers"]["u"]["autoApprove"],
        json!(["echo"])
    );

    // A disable and an enable in one session is a pair of switch flips: the
    // server's tools are held back rather than taken off, so nothing is rebuilt
    // and no server is dialled again. A rebuild would show up here as one more
    // start in `spawns` — the same evidence the rebuild test counts.
    let spawned = || {
        std::fs::read_to_string(env.spawns())
            .unwrap()
            .lines()
            .count()
    };
    let before = spawned();
    runtime
        .handle
        .mcp_act("u".into(), McpAction::Disable)
        .await
        .expect("disable");
    assert_eq!(user_config.json()["mcpServers"]["u"]["disabled"], true);
    runtime
        .handle
        .mcp_act("u".into(), McpAction::Enable)
        .await
        .expect("enable");
    assert_eq!(
        spawned(),
        before,
        "switching one server off and on rebuilds nothing"
    );
    let config = user_config.json();
    assert!(
        config["mcpServers"]["u"].get("disabled").is_none(),
        "{config}"
    );
    assert_eq!(
        config["mcpServers"]["u"]["env"]["TOKEN"], "s3cret",
        "the rest of the entry stays"
    );
    assert!(user_config.raw().unwrap().starts_with(SEAL));

    runtime
        .handle
        .mcp_act("h".into(), McpAction::Logout)
        .await
        .expect("sign out");
    let kept = tokens.plain();
    assert!(!kept.contains("[servers.h]"), "{kept}");
    assert!(
        kept.contains("[servers.other]"),
        "only that server's token goes: {kept}"
    );

    env.user_file_untouched();
    runtime.handle.shutdown().await.unwrap();
}

/// A retained connection is released without preparing the other servers again.
#[tokio::test(flavor = "multi_thread")]
async fn disable_then_enable_reuses_the_retained_connections() {
    let env = Env::new();
    let recorder = Arc::new(Recorder::default());
    let mut config: Value = serde_json::from_str(&env.user_config()).unwrap();
    config["mcpServers"]["neighbor"] = config["mcpServers"]["u"].clone();
    let user_config = Sealed::holding(&config.to_string());
    let tokens = Sealed::holding("");
    let mut runtime = started(env.start(&recorder, &user_config, &tokens)).await;
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(env.spawns())
            .unwrap()
            .lines()
            .count(),
        2
    );
    runtime
        .handle
        .mcp_act("u".into(), McpAction::Disable)
        .await
        .unwrap();
    turn(&mut runtime, "while disabled").await;
    assert!(!recorder
        .last_offered()
        .contains(&"mcp__u__echo".to_string()));
    assert!(recorder
        .last_offered()
        .contains(&"mcp__neighbor__echo".to_string()));
    runtime
        .handle
        .mcp_act("u".into(), McpAction::Enable)
        .await
        .unwrap();
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(env.spawns())
            .unwrap()
            .lines()
            .count(),
        2,
        "neither connection restarts"
    );
    turn(&mut runtime, "after enable").await;
    assert!(recorder
        .last_offered()
        .contains(&"mcp__u__echo".to_string()));
    runtime
        .handle
        .switch_tool("mcp__u__echo".into(), false)
        .await
        .unwrap();
    runtime
        .handle
        .mcp_act("u".into(), McpAction::Disable)
        .await
        .unwrap();
    runtime
        .handle
        .mcp_act("u".into(), McpAction::Enable)
        .await
        .unwrap();
    turn(&mut runtime, "manual tool switch stays off").await;
    assert!(!recorder
        .last_offered()
        .contains(&"mcp__u__echo".to_string()));
    assert_eq!(
        std::fs::read_to_string(env.spawns())
            .unwrap()
            .lines()
            .count(),
        2
    );
    env.user_file_untouched();
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn enabling_a_server_disabled_at_start_still_prepares_it() {
    let env = Env::new();
    let recorder = Arc::new(Recorder::default());
    let mut config: Value = serde_json::from_str(&env.user_config()).unwrap();
    config["mcpServers"]["u"]["disabled"] = json!(true);
    let user_config = Sealed::holding(&config.to_string());
    let tokens = Sealed::holding("");
    let mut runtime = started(env.start(&recorder, &user_config, &tokens)).await;
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    assert!(!env.spawns().exists());
    runtime
        .handle
        .mcp_act("u".into(), McpAction::Enable)
        .await
        .unwrap();
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    turn(&mut runtime, "after enable").await;
    assert!(recorder
        .last_offered()
        .contains(&"mcp__u__echo".to_string()));
    assert_eq!(
        std::fs::read_to_string(env.spawns())
            .unwrap()
            .lines()
            .count(),
        1
    );
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_config_and_reprepared_holds_are_not_reused() {
    for reprepare in [false, true] {
        let env = Env::new();
        let recorder = Arc::new(Recorder::default());
        let user_config = Sealed::holding(&env.user_config());
        let tokens = Sealed::holding("");
        let mut runtime = started(env.start(&recorder, &user_config, &tokens)).await;
        runtime
            .handle
            .wait_mcp_ready(std::time::Duration::from_secs(10))
            .await
            .unwrap();
        // An explicit on-exception to a broader manual off-pattern must
        // survive restoring this Disable's own marks into a replacement.
        runtime
            .handle
            .switch_tool("mcp__u__*".into(), false)
            .await
            .unwrap();
        runtime
            .handle
            .switch_tool("mcp__u__echo".into(), true)
            .await
            .unwrap();
        runtime
            .handle
            .mcp_act("u".into(), McpAction::Disable)
            .await
            .unwrap();
        if reprepare {
            runtime.handle.reload_capabilities().await.unwrap();
            runtime
                .handle
                .wait_mcp_ready(std::time::Duration::from_secs(10))
                .await
                .unwrap();
        } else {
            let mut changed = user_config.json();
            changed["mcpServers"]["u"]["env"]["TOKEN"] = json!("changed");
            *user_config.stored.lock().unwrap() = Some(format!("{SEAL}{changed}"));
        }
        runtime
            .handle
            .mcp_act("u".into(), McpAction::Enable)
            .await
            .unwrap();
        runtime
            .handle
            .wait_mcp_ready(std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(env.spawns())
                .unwrap()
                .lines()
                .count(),
            2,
            "a replacement was connected: reprepare={reprepare}"
        );
        turn(&mut runtime, "after enable").await;
        assert!(
            recorder
                .last_offered()
                .contains(&"mcp__u__echo".to_string()),
            "owned switch marks do not strand replacement tools"
        );
        runtime.handle.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn enabling_a_retained_server_rechecks_project_trust() {
    let env = Env::new();
    let recorder = Arc::new(Recorder::default());
    let config: Value = serde_json::from_str(&env.user_config()).unwrap();
    std::fs::write(
        env.project.path().join(".mcp.json"),
        json!({"mcpServers": {"p": config["mcpServers"]["u"].clone()}}).to_string(),
    )
    .unwrap();
    atomcode_capabilities::mcp::trust::trust_project(env.project.path(), env.home.path()).unwrap();
    let user_config = Sealed::holding(&config.to_string());
    let tokens = Sealed::holding("");
    let mut runtime = started(env.start(&recorder, &user_config, &tokens)).await;
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    runtime
        .handle
        .mcp_act("p".into(), McpAction::Disable)
        .await
        .unwrap();
    atomcode_capabilities::mcp::trust::untrust_project(env.project.path(), env.home.path())
        .unwrap();
    runtime
        .handle
        .mcp_act("p".into(), McpAction::Enable)
        .await
        .unwrap();
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    turn(&mut runtime, "project trust was withdrawn").await;
    assert!(!recorder
        .last_offered()
        .contains(&"mcp__p__echo".to_string()));
    assert!(recorder
        .last_offered()
        .contains(&"mcp__u__echo".to_string()));
    runtime.handle.shutdown().await.unwrap();
}

/// A host document the runtime cannot read is an error naming it, never an empty
/// config: the runtime starts, nothing connects, the `/mcp` rows say why, and an
/// edit stops before writing — the document is left as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_kept_config_that_cannot_be_read_fails_and_is_left_as_it_is() {
    let env = Env::new();
    let recorder = Arc::new(Recorder::default());
    let user_config = Arc::new(Sealed::default());
    *user_config.stored.lock().unwrap() = Some("sealed with another key".to_string());
    let tokens = Arc::new(Sealed::default());
    let mut runtime = started(env.start(&recorder, &user_config, &tokens)).await;
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();

    turn(&mut runtime, "hello").await;
    assert!(
        !recorder
            .last_offered()
            .iter()
            .any(|t| t.starts_with("mcp__")),
        "nothing connected: {:?}",
        recorder.last_offered()
    );
    let error = runtime.handle.mcp_rows().await.unwrap_err().to_string();
    assert!(
        error.contains("the user-level MCP config the host keeps")
            && error.contains("not sealed with this key"),
        "{error}"
    );
    let error = runtime
        .handle
        .mcp_act("u".into(), McpAction::Disable)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the user-level MCP config the host keeps"),
        "{error}"
    );
    assert_eq!(
        user_config.raw().as_deref(),
        Some("sealed with another key"),
        "nothing was written over it"
    );
    assert_eq!(*user_config.updates.lock().unwrap(), 0);
    env.user_file_untouched();
    runtime.handle.shutdown().await.unwrap();
}

/// Real runtime + stdio discovery: schemas stay off the first provider request,
/// search exposes one on the next round, and execution still uses the MCP alias.
/// Wall-clock test because it coordinates an actual subprocess over pipes.
#[tokio::test(flavor = "multi_thread")]
async fn a_large_mcp_catalog_is_searched_then_called_without_schema_history_bloat() {
    run_mcp_discovery(12_000).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn default_discovery_searches_small_catalogs_before_calling_original_tools() {
    run_mcp_discovery(600).await;
}

async fn run_mcp_discovery(schema_chars: usize) {
    use atomcode_kernel::tool::ToolCall;
    let env = Env::new();
    let recorder = Arc::new(Recorder::default());
    let tools = (0..34).map(|n| json!({
        "name": format!("inspect_{n:02}"), "description":"inspect reverse engineering function",
        "inputSchema":{"type":"object","properties":{"address":{"type":"string","description":"x".repeat(schema_chars)}}}
    })).collect::<Vec<_>>();
    let script = write_mcp_server_with_tools(env.scratch.path(), &env.spawns(), json!(tools));
    let user_config = Sealed::holding(
        &json!({"mcpServers":{"rea":{"command":"sh","args":[script],"trust":true}}}).to_string(),
    );
    let tokens = Sealed::holding("");
    for (id, name, arguments) in [
        (
            "discover",
            "mcp_search",
            r#"{"query":"mcp__rea__inspect_17","limit":1}"#,
        ),
        ("inspect", "mcp__rea__inspect_17", r#"{"address":"0x1234"}"#),
    ] {
        recorder.steps.lock().unwrap().push_back(vec![
            StreamEvent::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            }),
            StreamEvent::Done { truncated: false },
        ]);
    }
    let mut start = env.start(&recorder, &user_config, &tokens);
    start.agent.mcp_tool_search = Default::default();
    let mut runtime = started(start).await;
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    turn(&mut runtime, "inspect the function with RE tools").await;
    let requests = recorder.requests.lock().unwrap().clone();
    assert!(
        requests.len() >= 3,
        "discovery, execution, answer: {}",
        requests.len()
    );
    assert!(requests[0].0.iter().any(|t| t.name == "mcp_search"));
    assert!(!requests[0]
        .0
        .iter()
        .any(|t| t.name.starts_with("mcp__rea__")));
    let selected = requests[1]
        .0
        .iter()
        .filter(|t| t.name.starts_with("mcp__rea__"))
        .collect::<Vec<_>>();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name, "mcp__rea__inspect_17");
    assert!(selected[0].parameters.to_string().len() > schema_chars);
    let search_result = requests[1]
        .1
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("discover"))
        .unwrap();
    assert!(!search_result.is_error, "{}", search_result.text);
    assert!(search_result.text.len() < 2_048);
    assert!(requests[2]
        .1
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("inspect")
            && !m.is_error
            && m.text.contains("echo:from-server")));
    assert_eq!(
        runtime
            .handle
            .mcp_rows()
            .await
            .unwrap()
            .rows
            .iter()
            .find(|row| row.name == "rea")
            .unwrap()
            .tool_count,
        34
    );
    let mut eager = env.agent();
    eager.mcp_tool_search = atomcode_config::config::McpToolSearchMode::Off;
    runtime.handle.reprepare_config(eager).await.unwrap();
    runtime
        .handle
        .wait_mcp_ready(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    turn(&mut runtime, "now use eager tools").await;
    assert_eq!(
        recorder
            .last_offered()
            .iter()
            .filter(|name| name.starts_with("mcp__rea__"))
            .count(),
        34
    );
    assert!(!recorder
        .last_offered()
        .iter()
        .any(|name| name == "mcp_search"));
    runtime.handle.shutdown().await.unwrap();
    runtime.task.await.unwrap();
    env.user_file_untouched();
}
