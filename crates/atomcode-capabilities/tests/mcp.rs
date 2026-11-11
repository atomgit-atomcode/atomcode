//! End-to-end tests for the `mcp` capability, using the in-tree `mcp-test-server`
//! stdio fixture (a minimal MCP server: `initialize` + `tools/list` [one `echo`
//! tool] + `tools/call`). Exercises the real ported transport/registry, the kernel
//! `Tool` adapter, and the kernel Tool-contract conformance gate.
#![cfg(feature = "mcp")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use atomcode_capabilities::mcp::config::{McpConfigSource, McpServerConfig, McpTransportConfig};
use atomcode_capabilities::mcp::{McpRegistry, McpToolAdapter, ServerStatus, CONNECT_TIMEOUT};
use atomcode_kernel::conformance;
use atomcode_kernel::tool::{ProgressSink, RiskLevel, Tool, ToolContext};
use tokio_util::sync::CancellationToken;

/// A user tree nothing here reads: these tests add servers directly.
const NO_TREE: &str = "/nonexistent/user-tree";

/// A stdio server config pointing at the in-tree `mcp-test-server` fixture binary.
fn test_server_config(name: &str) -> McpServerConfig {
    test_server_config_with_env(name, BTreeMap::new())
}

fn test_server_config_with_env(name: &str, env: BTreeMap<String, String>) -> McpServerConfig {
    test_server_config_with_env_and_timeout(name, env, 5_000)
}

fn test_server_config_with_env_and_timeout(
    name: &str,
    env: BTreeMap<String, String>,
    timeout_ms: u64,
) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        source: McpConfigSource::Project,
        disabled: false,
        config: McpTransportConfig::Stdio {
            command: env!("CARGO_BIN_EXE_mcp-test-server").to_string(),
            args: vec![],
            env,
            timeout_ms: Some(timeout_ms),
        },
        trust: false,
        auto_approve: vec![],
        allowed_tools: None,
        required_experimental_capabilities: Default::default(),
    }
}

fn result_text(result: atomcode_capabilities::mcp::CallToolResult) -> String {
    assert!(!result.is_error);
    result
        .content
        .into_iter()
        .filter_map(|block| match block {
            atomcode_capabilities::mcp::ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn spawn_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|contents| contents.lines().count())
        .unwrap_or_default()
}

fn ctx() -> ToolContext {
    ToolContext {
        working_dir: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        progress: ProgressSink::noop(),
        requester: None,
    }
}

/// Discovery, remount cache and direct/stale adapter calls share the same policy.
#[tokio::test]
async fn allowed_tools_filter_discovery_and_execution_without_granting_approval() {
    let registry = McpRegistry::new(NO_TREE).share();
    let mut config = test_server_config("restricted");
    config.allowed_tools = Some(vec!["echo".into()]);
    registry.add_server(config.clone()).await.unwrap();
    let infos = registry.list_all_tools().await;
    assert_eq!(infos.len(), 1);
    let adapter = McpToolAdapter::new(registry.clone(), infos[0].clone()).unwrap();
    assert_eq!(adapter.risk("{}"), RiskLevel::Risky);
    assert!(
        !adapter
            .execute(r#"{"message":"allowed"}"#, &ctx())
            .await
            .is_error
    );
    assert_eq!(registry.listed_tools().len(), 1);

    // Even a trusted server and an already-created adapter cannot bypass deny-all.
    config.allowed_tools = Some(vec![]);
    config.trust = true;
    registry.add_server(config.clone()).await.unwrap();
    assert!(registry.listed_tools().is_empty());
    assert!(registry.list_all_tools().await.is_empty());
    assert!(registry
        .list_tools_for_server("restricted")
        .await
        .is_empty());
    let denied = adapter.execute(r#"{"message":"blocked"}"#, &ctx()).await;
    assert!(denied.is_error && denied.content.contains("blocked by allowedTools"));
    assert!(registry
        .call_tool("restricted", "echo", serde_json::json!({}))
        .await
        .is_err());

    config.allowed_tools = Some(vec!["*".into(), "mcp__restricted__echo".into()]);
    registry.add_server(config.clone()).await.unwrap();
    assert!(
        registry.list_all_tools().await.is_empty(),
        "only exact raw tool names match"
    );
    config.allowed_tools = None;
    registry.add_server(config).await.unwrap();
    assert_eq!(
        registry.list_all_tools().await.len(),
        1,
        "omitted policy preserves existing behavior"
    );
}

#[test]
fn allowed_tools_config_distinguishes_absent_empty_and_invalid() {
    use atomcode_capabilities::mcp::parse_mcp_servers;
    let parse = |extra: &str| {
        parse_mcp_servers(
            &format!(r#"{{"mcpServers":{{"b":{{"command":"node"{extra}}}}}}}"#),
            McpConfigSource::User,
        )
    };
    assert_eq!(parse("").unwrap()[0].allowed_tools, None);
    assert_eq!(
        parse(r#", "allowedTools": []"#).unwrap()[0].allowed_tools,
        Some(vec![])
    );
    assert_eq!(
        parse(r#", "allowed_tools": ["echo"]"#).unwrap()[0].allowed_tools,
        Some(vec!["echo".into()])
    );
    assert!(parse(r#", "allowedTools": "echo""#).is_err());
    assert!(parse(r#", "allowedTools": [42]"#).is_err());
}

/// The core happy path: connect a stdio server, discover its tool, wrap it as a
/// kernel `Tool`, and call it — asserting the `mcp__{server}__{tool}` naming, the
/// always-`Risky` classification, and the round-tripped echo output.
#[tokio::test]
async fn registry_connect_discover_and_call_echo() {
    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(test_server_config("testsrv"))
        .await
        .expect("stdio MCP server should connect");
    let registry = registry.share();

    let infos = registry.list_all_tools().await;
    assert_eq!(infos.len(), 1, "test server exposes exactly one tool");
    assert_eq!(infos[0].tool_name, "echo");

    let adapter = McpToolAdapter::new(registry, infos.into_iter().next().unwrap()).unwrap();
    assert_eq!(adapter.name(), "mcp__testsrv__echo");
    assert_eq!(
        adapter.risk("{}"),
        RiskLevel::Risky,
        "external MCP tools must always be Risky so approval middleware gates them"
    );

    let result = adapter.execute(r#"{"message":"hi"}"#, &ctx()).await;
    assert!(!result.is_error, "echo call should succeed: {result:?}");
    assert_eq!(result.content, "echo:hi");
}

/// Regression-style reproduction: a stdio server whose `args` contain spaces and
/// non-ASCII characters (a script path under a directory with a CJK name). The
/// arg is written as `${ATOMCODE_TEST_MCP_UNICODE_DEFAULT:-<cjk path>}` in `.mcp.json` — the variable
/// is deliberately never set, so the value reaches argv through the default
/// branch of `load_mcp_config` → `expand_env_vars` — a regression of the
/// byte-wise expansion fails the byte-for-byte assertion below, before any
/// connection is attempted. No process env is mutated, so the test cannot race
/// other tests' environ reads. Connect and reconnect then also exercise such
/// args in argv (the fixture ignores extra args, so they are not the guard).
#[tokio::test]
async fn stdio_survives_args_with_spaces_and_unicode_across_reconnect() {
    const UNICODE_PATH: &str = "/tmp/中文 目录/with space/server.py";
    // The assertion below expects the default branch of ${ATOMCODE_TEST_MCP_UNICODE_DEFAULT:-…} to
    // be taken. This is a read-only check (no setenv, no race); a host that
    // defines this test-namespaced variable would invalidate the expectation,
    // so refuse loudly instead of failing obscurely mid-test.
    assert!(
        std::env::var("ATOMCODE_TEST_MCP_UNICODE_DEFAULT").is_err(),
        "the host must not define ATOMCODE_TEST_MCP_UNICODE_DEFAULT — it would override the \
         ${{ATOMCODE_TEST_MCP_UNICODE_DEFAULT:-…}} default this test asserts on"
    );
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "spacey-args": {
                    "command": env!("CARGO_BIN_EXE_mcp-test-server"),
                    // Never set ATOMCODE_TEST_MCP_UNICODE_DEFAULT anywhere: the default branch of
                    // ${VAR:-default} carries the CJK value through expansion.
                    "args": ["${ATOMCODE_TEST_MCP_UNICODE_DEFAULT:-/tmp/中文 目录/with space/server.py}"]
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let servers = atomcode_capabilities::mcp::config::load_mcp_config(
        project.path(),
        std::path::Path::new("/nonexistent/user"),
    )
    .expect("the config must parse");
    assert_eq!(servers.len(), 1, "the server must be admitted: {servers:?}");
    // The fixture server ignores extra args, so connect alone cannot catch a
    // corrupted expansion — assert the expanded bytes before connecting.
    let McpTransportConfig::Stdio { args, .. } = &servers[0].config else {
        panic!(
            "a `command` entry must load as Stdio: {:?}",
            servers[0].config
        );
    };
    assert_eq!(
        args.as_slice(),
        [UNICODE_PATH],
        "expansion must preserve non-ASCII bytes byte for byte"
    );

    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(servers[0].clone())
        .await
        .expect("connect with expanded spaces/unicode args should succeed");
    registry
        .reconnect_server(servers[0].clone())
        .await
        .expect("reconnect with expanded spaces/unicode args should succeed");
    assert_eq!(
        registry.server_statuses().await,
        vec![("spacey-args".to_string(), ServerStatus::Connected)]
    );
}

/// Regression for a config copied from tools like Claude/Cursor: unknown fields
/// (`type`, `timeout`, `description`) plus an args path containing spaces and
/// CJK characters. Loads it through the real config parser — where `${VAR}`
/// expansion must not corrupt non-ASCII bytes — and connects through the real
/// registry. Scope note: this bypasses the daemon's project-trust gating (the
/// registry admits the server directly), so it covers load + connect, not the
/// trust flow.
#[tokio::test]
async fn config_with_unknown_fields_and_unicode_path_loads_and_connects() {
    let unicode_arg = "/tmp/中文 目录/with space/server.py";
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "unicode-cfg": {
                    "type": "stdio",
                    "command": env!("CARGO_BIN_EXE_mcp-test-server"),
                    "args": [unicode_arg],
                    "timeout": 1200000,
                    "description": "copied from another tool"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let servers = atomcode_capabilities::mcp::config::load_mcp_config(
        project.path(),
        std::path::Path::new("/nonexistent/user"),
    )
    .expect("config with unknown fields must parse");
    assert_eq!(servers.len(), 1, "the server must be admitted: {servers:?}");
    let McpTransportConfig::Stdio { args, .. } = &servers[0].config else {
        panic!(
            "a `command` entry must load as Stdio: {:?}",
            servers[0].config
        );
    };
    assert_eq!(
        args.as_slice(),
        [unicode_arg],
        "expansion must preserve non-ASCII bytes byte for byte"
    );

    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(servers.into_iter().next().unwrap())
        .await
        .expect("a unicode-path config should connect through the real load path");
}

#[tokio::test]
async fn stdio_reconnects_once_after_server_exit_for_concurrent_calls() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("exit-once");
    let counter = temp.path().join("spawn-count");
    let env = BTreeMap::from([
        (
            "MCP_TEST_EXIT_ONCE_MARKER".to_string(),
            marker.display().to_string(),
        ),
        (
            "MCP_TEST_SPAWN_COUNTER".to_string(),
            counter.display().to_string(),
        ),
    ]);

    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(test_server_config_with_env("recover", env))
        .await
        .expect("stdio MCP server should connect");
    let registry = registry.share();
    assert_eq!(spawn_count(&counter), 1);

    let first = registry.call_tool("recover", "echo", serde_json::json!({ "message": "first" }));
    let second = registry.call_tool(
        "recover",
        "echo",
        serde_json::json!({ "message": "second" }),
    );
    let (first, second) = tokio::join!(first, second);

    let outcomes = [first, second];
    assert_eq!(
        outcomes.iter().filter(|result| result.is_ok()).count(),
        1,
        "the call handled by the replacement process should succeed"
    );
    let uncertain = outcomes
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("the call sent to the dying process must not be replayed");
    assert!(
        uncertain.to_string().contains("result is unknown"),
        "unexpected error: {uncertain:#}"
    );
    assert_eq!(
        spawn_count(&counter),
        2,
        "initial process plus exactly one replacement should be spawned"
    );
    assert_eq!(
        registry.server_statuses().await,
        vec![("recover".to_string(), ServerStatus::Connected)]
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stdio_timeout_uses_one_deadline_without_replaying_tool() {
    let _clock = ExternalIoClock::hold();
    let temp = tempfile::tempdir().unwrap();
    let read_gate = temp.path().join("read-gate");
    let response_gate = temp.path().join("response-gate");
    let read_started = temp.path().join("read-started");
    let calls = temp.path().join("tool-call-count");
    let spawns = temp.path().join("spawn-count");
    let reconnect_delay = temp.path().join("delay-reconnect-initialize");
    let env = BTreeMap::from([
        (
            "MCP_TEST_READ_GATE".into(),
            read_gate.to_string_lossy().into_owned(),
        ),
        (
            "MCP_TEST_READ_STARTED".into(),
            read_started.to_string_lossy().into_owned(),
        ),
        (
            "MCP_TEST_TOOL_RESPONSE_GATE".into(),
            response_gate.to_string_lossy().into_owned(),
        ),
        (
            "MCP_TEST_TOOL_CALL_COUNTER".to_string(),
            calls.display().to_string(),
        ),
        (
            "MCP_TEST_SPAWN_COUNTER".to_string(),
            spawns.display().to_string(),
        ),
        (
            "MCP_TEST_INITIALIZE_DELAY_MARKER".to_string(),
            reconnect_delay.display().to_string(),
        ),
        (
            "MCP_TEST_INITIALIZE_DELAY_MS".to_string(),
            "300".to_string(),
        ),
    ]);
    let registry = McpRegistry::new(NO_TREE);
    tokio::time::resume();
    registry
        .add_server(test_server_config_with_env_and_timeout(
            "shared-deadline",
            env,
            500,
        ))
        .await
        .expect("stdio MCP server should connect");
    wait_for_tool_counter(read_started, 1).await;
    std::fs::write(&reconnect_delay, "delay replacement only").unwrap();
    tokio::time::pause();

    let message = "x".repeat(4 * 1024 * 1024);
    let started = tokio::time::Instant::now();
    let call = registry.call_tool(
        "shared-deadline",
        "echo",
        serde_json::json!({ "message": message }),
    );
    tokio::pin!(call);
    assert!(
        futures::poll!(call.as_mut()).is_pending(),
        "4 MiB cannot fit in the blocked pipe"
    );
    tokio::time::advance(Duration::from_millis(300)).await;
    assert!(
        !calls.exists(),
        "the server cannot read the request before its gate opens"
    );
    std::fs::write(read_gate, "read request").unwrap();
    tokio::select! {
        result = call.as_mut() => panic!("response gate is still closed: {result:?}"),
        _ = wait_for_tool_counter(calls.clone(), 1) => {}
    }
    tokio::time::advance(Duration::from_millis(201)).await;
    let result = match futures::poll!(call.as_mut()) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => panic!("write and response must share the original 500ms deadline; foreground must not await recovery"),
    };
    let error = result.expect_err("the response gate is still closed");
    assert!(error.to_string().contains("result is unknown"), "{error:#}");
    assert_eq!(started.elapsed(), Duration::from_millis(501));
    std::fs::write(response_gate, "late response").unwrap();
    tokio::time::resume();
    assert_eq!(spawn_count(&calls), 1, "the tool must run only once");

    let mut recovered = false;
    for _ in 0..100 {
        if registry.server_statuses().await
            == vec![("shared-deadline".to_string(), ServerStatus::Connected)]
        {
            recovered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        recovered,
        "stdio connection should recover in the background"
    );
    assert_eq!(
        registry
            .list_tools_for_server("shared-deadline")
            .await
            .len(),
        1,
        "the recovered process must remain usable after the recovery task exits"
    );
    assert_eq!(spawn_count(&spawns), 2, "the client should only reconnect");
    registry.shutdown().await;
}

#[tokio::test]
async fn stdio_marks_server_failed_when_retry_connection_also_dies() {
    let temp = tempfile::tempdir().unwrap();
    let counter = temp.path().join("spawn-count");
    let env = BTreeMap::from([
        (
            "MCP_TEST_EXIT_ON_EVERY_TOOLS_LIST".to_string(),
            "1".to_string(),
        ),
        (
            "MCP_TEST_SPAWN_COUNTER".to_string(),
            counter.display().to_string(),
        ),
    ]);

    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(test_server_config_with_env("always-dies", env))
        .await
        .expect("stdio MCP server should connect");

    assert!(
        registry
            .list_tools_for_server("always-dies")
            .await
            .is_empty(),
        "tools/list should fail after exactly one reconnect"
    );
    assert_eq!(spawn_count(&counter), 2);
    assert!(matches!(
        registry.server_statuses().await.as_slice(),
        [(name, ServerStatus::Failed(_))] if name == "always-dies"
    ));
}

#[tokio::test]
async fn concurrent_failures_reconnect_the_generation_that_actually_failed() {
    let temp = tempfile::tempdir().unwrap();
    let exits = temp.path().join("remaining-exits");
    let counter = temp.path().join("spawn-count");
    std::fs::write(&exits, "2").unwrap();
    let env = BTreeMap::from([
        (
            "MCP_TEST_EXIT_TOOL_CALLS_COUNTER".to_string(),
            exits.display().to_string(),
        ),
        (
            "MCP_TEST_SPAWN_COUNTER".to_string(),
            counter.display().to_string(),
        ),
    ]);

    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(test_server_config_with_env("two-exits", env))
        .await
        .expect("stdio MCP server should connect");
    let registry = registry.share();

    let first = registry.call_tool(
        "two-exits",
        "echo",
        serde_json::json!({ "message": "first" }),
    );
    let second = registry.call_tool(
        "two-exits",
        "echo",
        serde_json::json!({ "message": "second" }),
    );
    let (first, second) = tokio::join!(first, second);

    for result in [first, second] {
        let error = result.expect_err("fixture should terminate both tool calls");
        assert!(
            error.to_string().contains("result is unknown"),
            "sent tool calls must not be replayed: {error:#}"
        );
    }
    let mut recovered = false;
    for _ in 0..100 {
        if spawn_count(&counter) == 3
            && registry.server_statuses().await
                == vec![("two-exits".to_string(), ServerStatus::Connected)]
        {
            recovered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        recovered,
        "each distinct failed generation should get one replacement"
    );
}

#[tokio::test]
async fn status_detects_an_exited_child_before_the_next_request() {
    let env = BTreeMap::from([(
        "MCP_TEST_EXIT_AFTER_INITIALIZED".to_string(),
        "1".to_string(),
    )]);
    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(test_server_config_with_env("already-exited", env))
        .await
        .expect("initialize handshake should complete before fixture exits");

    let mut observed = false;
    for _ in 0..20 {
        if matches!(
            registry.server_statuses().await.as_slice(),
            [(name, ServerStatus::Failed(_))] if name == "already-exited"
        ) {
            observed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        observed,
        "status should inspect the child instead of reporting a stale Connected state"
    );
}

/// A malformed-arguments call must surface as a tool error (`is_error`), never a
/// panic — the kernel PANIC CONTRACT.
#[tokio::test]
async fn adapter_maps_bad_arguments_to_tool_error() {
    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(test_server_config("testsrv"))
        .await
        .expect("stdio MCP server should connect");
    let registry = registry.share();
    let infos = registry.list_all_tools().await;
    let adapter = McpToolAdapter::new(registry, infos.into_iter().next().unwrap()).unwrap();

    let result = adapter.execute("not json", &ctx()).await;
    assert!(
        result.is_error,
        "invalid JSON args must become a tool error"
    );
    assert!(result.content.contains("invalid MCP tool arguments"));
}

/// Every discovered MCP tool must satisfy the kernel `Tool` contract (stable
/// name/description/schema, deterministic risk, execute that terminates without
/// panicking). This is the gate the spec requires for each surfaced tool.
#[tokio::test]
async fn adapter_passes_kernel_tool_conformance() {
    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(test_server_config("conf"))
        .await
        .expect("stdio MCP server should connect");
    let registry = registry.share();
    let infos = registry.list_all_tools().await;
    let adapter: Arc<dyn Tool> =
        Arc::new(McpToolAdapter::new(registry, infos.into_iter().next().unwrap()).unwrap());

    let report = conformance::tool::check(adapter, &[r#"{"message":"x"}"#]).await;
    report.assert_conformant();
}

/// Write a trust store file that marks `project_dir` as trusted.
/// Mirrors the format written by `mcp::trust::trust_project`.
fn write_trusted_store(store_path: &std::path::Path, project_dir: &std::path::Path) {
    let key = atomcode_capabilities::mcp::registry::project_trust_key(project_dir);
    let store = serde_json::json!({
        "version": 1,
        "projects": {
            key: { "path": project_dir.display().to_string() }
        }
    });
    std::fs::write(store_path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
}

/// Background config loading discovers a trusted project's tools without imposing
/// session-transition policy on the capability layer.
#[tokio::test]
async fn background_registry_reads_project_mcp_json() {
    // An empty user tree (no user mcp.json), handed in.
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // Pre-trust the project in that tree's store so the security gate allows it.
    let trust_store = home.path().join("mcp_trust.json");
    write_trusted_store(&trust_store, project.path());

    let server = env!("CARGO_BIN_EXE_mcp-test-server");
    let mcp_json = serde_json::json!({
        "mcpServers": { "proj": { "command": server, "args": [], "timeout_ms": 5000 } }
    });
    std::fs::write(project.path().join(".mcp.json"), mcp_json.to_string()).unwrap();

    let registry = McpRegistry::from_config_background(project.path(), home.path()).share();
    registry.wait_for_initial_connections(CONNECT_TIMEOUT).await;
    let adapters: Vec<Arc<dyn Tool>> = registry
        .list_all_tools()
        .await
        .into_iter()
        .map(|info| Arc::new(McpToolAdapter::new(registry.clone(), info).unwrap()) as Arc<dyn Tool>)
        .collect();

    let names: Vec<String> = adapters.iter().map(|a| a.name().to_string()).collect();
    assert!(
        names.iter().any(|n| n == "mcp__proj__echo"),
        "background discovery should surface the project server's echo tool; got {names:?}"
    );
    let statuses = registry.server_statuses().await;
    assert!(
        statuses.iter().any(|(n, _)| n == "proj"),
        "the connected server should appear in server_statuses"
    );
}

/// Real stdio processes: replacing one server must not restart its neighbor.
#[tokio::test]
async fn manual_reconnect_replaces_only_the_selected_server() {
    let temp = tempfile::tempdir().unwrap();
    let a_spawns = temp.path().join("a-spawns");
    let b_spawns = temp.path().join("b-spawns");
    let config = |name: &str, path: &std::path::Path| {
        test_server_config_with_env(
            name,
            BTreeMap::from([("MCP_TEST_SPAWN_COUNTER".into(), path.display().to_string())]),
        )
    };
    let registry = McpRegistry::new(NO_TREE);
    let a = config("a", &a_spawns);
    registry.add_server(a.clone()).await.unwrap();
    registry.add_server(config("b", &b_spawns)).await.unwrap();
    registry.list_all_tools().await;
    registry.reconnect_server(a).await.unwrap();
    let tools = registry.list_tools_for_server("a").await;
    assert_eq!(tools.len(), 1);
    assert_eq!(spawn_count(&a_spawns), 2);
    assert_eq!(spawn_count(&b_spawns), 1);
    let result = registry
        .call_tool("b", "echo", serde_json::json!({"message":"still here"}))
        .await
        .unwrap();
    assert!(result_text(result).contains("still here"));
}

#[tokio::test]
async fn failed_manual_reconnect_discards_old_tools_and_can_be_retried() {
    let registry = McpRegistry::new(NO_TREE);
    let good = test_server_config("chosen");
    registry.add_server(good.clone()).await.unwrap();
    assert_eq!(registry.list_all_tools().await.len(), 1);
    let mut bad = good.clone();
    if let McpTransportConfig::Stdio { command, .. } = &mut bad.config {
        *command = "/nonexistent/mcp-reconnect-test".into();
    }
    assert!(registry.reconnect_server(bad).await.is_err());
    assert!(registry.listed_tools().is_empty());
    assert!(matches!(
        &registry.server_statuses().await[0].1,
        ServerStatus::Failed(_)
    ));
    assert!(registry
        .call_tool("chosen", "echo", serde_json::json!({}))
        .await
        .is_err());
    registry.reconnect_server(good).await.unwrap();
    assert_eq!(registry.list_tools_for_server("chosen").await.len(), 1);
    assert!(matches!(
        &registry.server_statuses().await[0].1,
        ServerStatus::Connected
    ));
}

#[tokio::test]
async fn manual_reconnect_reloads_the_selected_servers_approval_config() {
    let registry = McpRegistry::new(NO_TREE);
    let mut config = test_server_config("selected");
    config.trust = true;
    config.auto_approve = vec!["echo".into(), "mcp__neighbor__echo".into()];
    registry.add_server(config.clone()).await.unwrap();
    let mut neighbor = test_server_config("neighbor");
    neighbor.trust = true;
    neighbor.auto_approve = vec!["echo".into()];
    registry.add_server(neighbor).await.unwrap();
    assert!(registry.is_server_trusted("selected"));
    assert!(registry.is_tool_auto_approved("mcp__selected__echo"));
    config.trust = false;
    config.auto_approve.clear();
    registry.reconnect_server(config).await.unwrap();
    assert!(!registry.is_server_trusted("selected"));
    assert!(!registry.is_tool_auto_approved("mcp__selected__echo"));
    assert!(registry.is_server_trusted("neighbor"));
    assert!(registry.is_tool_auto_approved("mcp__neighbor__echo"));
}

/// Notifications and late responses must never become the current tool's result.
// Real subprocess I/O must progress before the operation deadline.
#[tokio::test]
async fn stdio_correlates_replies_past_notifications_and_stale_ids() {
    for framing in ["MCP_TEST_NDJSON", "MCP_TEST_CONTENT_LENGTH"] {
        let registry = McpRegistry::new(NO_TREE);
        registry
            .add_server(test_server_config_with_env(
                "correlated",
                BTreeMap::from([
                    ("MCP_TEST_NOTIFY_BEFORE_RESPONSE".into(), "1".into()),
                    ("MCP_TEST_STALE_RESPONSE".into(), "1".into()),
                    ("MCP_TEST_SERVER_REQUEST".into(), "1".into()),
                    (framing.into(), "1".into()),
                ]),
            ))
            .await
            .unwrap();
        for message in ["first", "second"] {
            let result = registry
                .call_tool("correlated", "echo", serde_json::json!({"message":message}))
                .await
                .unwrap();
            assert_eq!(result_text(result), format!("echo:{message}"));
        }
    }
}

// Real subprocess I/O must progress before the operation deadline.
#[tokio::test]
async fn malformed_stdio_result_is_not_replayed_or_received_by_the_next_call() {
    for failure_mode in [
        "MCP_TEST_MALFORMED_ONCE",
        "MCP_TEST_INVALID_UTF8_ONCE",
        "MCP_TEST_OVERSIZED_LENGTH_ONCE",
        "MCP_TEST_OVERSIZED_LINE_ONCE",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let calls = temp.path().join("calls");
        let registry = McpRegistry::new(NO_TREE);
        registry
            .add_server(test_server_config_with_env(
                "broken",
                BTreeMap::from([
                    (
                        failure_mode.into(),
                        temp.path().join("once").display().to_string(),
                    ),
                    (
                        "MCP_TEST_TOOL_CALL_COUNTER".into(),
                        calls.display().to_string(),
                    ),
                ]),
            ))
            .await
            .unwrap();
        let first = registry
            .call_tool("broken", "echo", serde_json::json!({"message":"first"}))
            .await;
        assert!(first.is_err());
        assert!(format!("{:#}", first.unwrap_err()).contains("result is unknown"));
        let second = registry
            .call_tool("broken", "echo", serde_json::json!({"message":"second"}))
            .await
            .unwrap();
        assert_eq!(result_text(second), "echo:second");
        assert_eq!(
            spawn_count(&calls),
            2,
            "one execution per explicit call, no hidden replay"
        );
    }
}

// Real subprocess pipes must make progress independently of the Tokio clock.
#[tokio::test]
async fn oversized_stdio_startup_response_fails_initialization() {
    let registry = McpRegistry::new(NO_TREE);
    let error = registry
        .add_server(test_server_config_with_env(
            "oversized-startup",
            BTreeMap::from([("MCP_TEST_OVERSIZED_STARTUP_LENGTH".into(), "1".into())]),
        ))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("Content-Length exceeds 10 MiB"));
}

// Real subprocess I/O must progress independently of the Tokio clock.
#[tokio::test]
async fn stdio_images_reach_the_tool_result_with_original_pixels_and_server_errors() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAIAAAB7QOjdAAAADUlEQVR4nGP4zwAE/wEHAAH/4iOeWQAAAABJRU5ErkJggg==";
    for is_error in [false, true] {
        let registry = McpRegistry::new(NO_TREE);
        let body = serde_json::json!({
            "content": [
                {"type":"text", "text":"captured the target window"},
                {"type":"image", "mimeType":"image/png", "data":PNG}
            ],
            "isError": is_error
        });
        registry
            .add_server(test_server_config_with_env(
                "screen",
                BTreeMap::from([("MCP_TEST_TOOL_RESULT".into(), body.to_string())]),
            ))
            .await
            .unwrap();
        let registry = registry.share();
        let adapter =
            McpToolAdapter::new(registry.clone(), registry.list_all_tools().await.remove(0))
                .unwrap();
        let output = adapter.execute("{}", &ctx()).await;
        assert_eq!(output.is_error, is_error, "{}", output.content);
        assert!(output.content.contains("captured the target window"));
        assert!(output.content.contains("2x1"));
        assert_eq!(output.images.len(), 1);
        assert_eq!(output.images[0].media_type, "image/png");
        assert_eq!(
            output.images[0].data, PNG,
            "screenshots must not be silently resized"
        );
    }
}

#[tokio::test]
async fn a_bad_mcp_image_is_reported_without_repeating_the_tool_call() {
    let temp = tempfile::tempdir().unwrap();
    let calls = temp.path().join("calls");
    let registry = McpRegistry::new(NO_TREE);
    registry
        .add_server(test_server_config_with_env(
            "screen",
            BTreeMap::from([
                (
                    "MCP_TEST_TOOL_RESULT".into(),
                    serde_json::json!({"content":[
                        {"type":"text", "text":"operation completed"},
                        {"type":"image", "mimeType":"image/png", "data":"not base64!"}
                    ]})
                    .to_string(),
                ),
                (
                    "MCP_TEST_TOOL_CALL_COUNTER".into(),
                    calls.display().to_string(),
                ),
            ]),
        ))
        .await
        .unwrap();
    let registry = registry.share();
    let adapter =
        McpToolAdapter::new(registry.clone(), registry.list_all_tools().await.remove(0)).unwrap();
    let output = adapter.execute("{}", &ctx()).await;
    assert!(output.is_error);
    assert!(output.images.is_empty());
    assert!(output.content.contains("operation completed"));
    assert!(output.content.contains("image output rejected"));
    assert!(!output.content.contains("not base64!"));
    assert_eq!(spawn_count(&calls), 1);
}

/// Opt-in compatibility check against the independently installed desktop
/// backend. Only initialize/tools/list are sent: no capture or GUI input.
/// Real subprocess scheduling must progress, so this does not use paused time.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires ATOMCODE_COMPUTER_TEST_EXECUTABLE; handshake/list only"]
async fn installed_computer_backend_exposes_scoped_tools_through_the_production_registry() {
    let executable = std::env::var("ATOMCODE_COMPUTER_TEST_EXECUTABLE")
        .expect("set ATOMCODE_COMPUTER_TEST_EXECUTABLE to an installed backend executable");
    let executable = std::fs::canonicalize(executable).unwrap();
    let user = tempfile::tempdir().unwrap();
    let registry = Arc::new(McpRegistry::new(user.path()));
    let mut env = BTreeMap::new();
    for flag in [
        "OPEN_COMPUTER_USE_ALLOW_GLOBAL_POINTER_FALLBACKS",
        "OPEN_COMPUTER_USE_MACOS_ALLOW_FOREGROUND_INPUT",
        "OPEN_COMPUTER_USE_WINDOWS_ALLOW_FOREGROUND_INPUT",
        "OPEN_COMPUTER_USE_WINDOWS_ALLOW_APP_LAUNCH",
        "OPEN_COMPUTER_USE_WINDOWS_ALLOW_FOCUS_ACTIONS",
    ] {
        env.insert(flag.into(), "0".into());
    }
    registry
        .add_server(McpServerConfig {
            name: "computer-use".into(),
            source: McpConfigSource::User,
            disabled: false,
            trust: false,
            auto_approve: vec![],
            allowed_tools: None,
            required_experimental_capabilities: Default::default(),
            config: McpTransportConfig::Stdio {
                command: executable.to_str().unwrap().into(),
                args: vec!["mcp".into()],
                env,
                timeout_ms: Some(10_000),
            },
        })
        .await
        .unwrap();
    let infos = registry.list_all_tools().await;
    for name in [
        "list_apps",
        "get_app_state",
        "list_windows",
        "get_window_state",
        "click",
        "type_text",
        "press_key",
    ] {
        assert!(
            infos.iter().any(|info| info.tool_name == name),
            "missing backend tool {name}"
        );
    }
    for info in infos {
        let name = info.tool_name.clone();
        let adapter = McpToolAdapter::new(registry.clone(), info).unwrap();
        if name == "click" {
            let schema = adapter.parameters_schema();
            assert!(schema["properties"].get("screenshotId").is_some());
            assert!(schema["properties"].get("window").is_some());
            assert_eq!(
                adapter.risk("{}"),
                RiskLevel::Risky,
                "mutations must retain native approval"
            );
        }
        if name == "get_window_state" {
            assert!(adapter.parameters_schema()["properties"]
                .get("include_screenshot")
                .is_some());
        }
    }
}

/// Keep external-process I/O on real time without letting a paused Tokio clock
/// manufacture an RPC timeout before the OS schedules the fixture. Tokio 1.50
/// inhibits automatic clock advance while spawn_blocking work is active. The
/// real watchdog releases that inhibition even if a test fails to finish.
struct ExternalIoClock(std::sync::mpsc::Sender<()>);
impl ExternalIoClock {
    fn hold() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        tokio::task::spawn_blocking(move || {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        });
        Self(tx)
    }
}
impl Drop for ExternalIoClock {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

async fn wait_for_tool_counter(path: std::path::PathBuf, count: usize) {
    tokio::task::spawn_blocking(move || {
        let start = std::time::Instant::now();
        while spawn_count(&path) < count && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            spawn_count(&path),
            count,
            "fixture reached the expected external effect"
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn dispatched_cancellation_and_forced_drop_require_explicit_reconnect_without_replay() {
    let _clock = ExternalIoClock::hold();
    for forced_drop in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let calls = temp.path().join("calls");
        let spawns = temp.path().join("spawns");
        let gate = temp.path().join("release");
        let finished = temp.path().join("finished");
        let env = BTreeMap::from([
            (
                "MCP_TEST_TOOL_CALL_COUNTER".into(),
                calls.to_str().unwrap().into(),
            ),
            (
                "MCP_TEST_SPAWN_COUNTER".into(),
                spawns.to_str().unwrap().into(),
            ),
            (
                "MCP_TEST_TOOL_RESPONSE_GATE".into(),
                gate.to_str().unwrap().into(),
            ),
            (
                "MCP_TEST_TOOL_FINISH_COUNTER".into(),
                finished.to_str().unwrap().into(),
            ),
        ]);
        let config = test_server_config_with_env("cancelled", env);
        let registry = McpRegistry::new(NO_TREE).share();
        // The startup noise-drain uses a real idle deadline; freeze only once
        // the external handshake has completed, not while it is draining.
        tokio::time::resume();
        registry.add_server(config.clone()).await.unwrap();
        tokio::time::pause();
        let info = registry.list_all_tools().await.remove(0);
        let adapter = Arc::new(McpToolAdapter::new(registry.clone(), info).unwrap());
        let cancel = CancellationToken::new();
        let task = {
            let tool = adapter.clone();
            let mut context = ctx();
            context.cancel = cancel.clone();
            tokio::spawn(async move { tool.execute(r#"{"message":"first"}"#, &context).await })
        };
        wait_for_tool_counter(calls.clone(), 1).await;
        if forced_drop {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            cancel.cancel();
            let result = task.await.unwrap();
            assert!(result.is_error && result.content.contains("unverified"));
        }
        assert!(
            matches!(&registry.server_statuses().await[0].1, ServerStatus::Failed(message) if message.contains("cancelled after dispatch"))
        );
        let blocked = adapter
            .execute(r#"{"message":"must not run"}"#, &ctx())
            .await;
        assert!(blocked.is_error && blocked.content.contains("Reconnect"));
        assert!(registry.list_tools_for_server("cancelled").await.is_empty());
        assert_eq!(spawn_count(&calls), 1);
        assert_eq!(
            spawn_count(&spawns),
            1,
            "no automatic replacement or replay"
        );
        std::fs::write(&gate, "release late response").unwrap();
        wait_for_tool_counter(finished, 1).await;
        assert!(
            adapter.execute("{}", &ctx()).await.is_error,
            "late success cannot unpoison the cancelled stream"
        );
        tokio::time::resume();
        registry.add_server(config).await.unwrap();
        tokio::time::pause();
        let result = adapter
            .execute(r#"{"message":"explicit new request"}"#, &ctx())
            .await;
        assert!(!result.is_error && result.content.contains("explicit new request"));
        assert_eq!(spawn_count(&calls), 2);
        assert_eq!(spawn_count(&spawns), 2);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn dropping_a_queued_stdio_call_does_not_poison_another_call_or_dispatch_it() {
    let _clock = ExternalIoClock::hold();
    let temp = tempfile::tempdir().unwrap();
    let calls = temp.path().join("calls");
    let gate = temp.path().join("release");
    let env = BTreeMap::from([
        (
            "MCP_TEST_TOOL_CALL_COUNTER".into(),
            calls.to_str().unwrap().into(),
        ),
        (
            "MCP_TEST_TOOL_RESPONSE_GATE".into(),
            gate.to_str().unwrap().into(),
        ),
    ]);
    let registry = McpRegistry::new(NO_TREE).share();
    tokio::time::resume();
    registry
        .add_server(test_server_config_with_env("queue", env))
        .await
        .unwrap();
    tokio::time::pause();
    let first = {
        let reg = registry.clone();
        tokio::spawn(async move {
            reg.call_tool("queue", "echo", serde_json::json!({"message":"first"}))
                .await
        })
    };
    wait_for_tool_counter(calls.clone(), 1).await;
    let queued = {
        let reg = registry.clone();
        tokio::spawn(async move {
            reg.call_tool("queue", "echo", serde_json::json!({"message":"never"}))
                .await
        })
    };
    tokio::task::yield_now().await;
    queued.abort();
    assert!(queued.await.unwrap_err().is_cancelled());
    std::fs::write(gate, "release first response").unwrap();
    assert!(result_text(first.await.unwrap().unwrap()).contains("first"));
    assert!(matches!(
        &registry.server_statuses().await[0].1,
        ServerStatus::Connected
    ));
    assert!(result_text(
        registry
            .call_tool("queue", "echo", serde_json::json!({"message":"next"}))
            .await
            .unwrap()
    )
    .contains("next"));
    assert_eq!(
        spawn_count(&calls),
        2,
        "queued cancelled call was never sent"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancelled_http_request_cannot_be_unpoisoned_by_a_late_response() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let _clock = ExternalIoClock::hold();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(tokio::sync::Notify::new());
    let parallel_entered = Arc::new(tokio::sync::Notify::new());
    let parallel_release = Arc::new(tokio::sync::Notify::new());
    let server = {
        let calls = calls.clone();
        let entered = entered.clone();
        let release = release.clone();
        let finished = finished.clone();
        let parallel_entered = parallel_entered.clone();
        let parallel_release = parallel_release.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let calls = calls.clone();
                let entered = entered.clone();
                let release = release.clone();
                let finished = finished.clone();
                let parallel_entered = parallel_entered.clone();
                let parallel_release = parallel_release.clone();
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    let mut size = 0;
                    loop {
                        line.clear();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                size = value.trim().parse::<usize>().unwrap();
                            }
                        }
                    }
                    let mut body = vec![0; size];
                    stream.read_exact(&mut body).await.unwrap();
                    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let method = request["method"].as_str().unwrap_or("");
                    let mut first_tool = false;
                    let result = match method {
                        "initialize" => {
                            serde_json::json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"http-cancel","version":"1"}})
                        }
                        "tools/list" => {
                            serde_json::json!({"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]})
                        }
                        "tools/call" => {
                            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                            if n == 1 {
                                first_tool = true;
                                entered.notify_one();
                                release.notified().await;
                            }
                            if n == 2 {
                                parallel_entered.notify_one();
                                parallel_release.notified().await;
                            }
                            serde_json::json!({"content":[{"type":"text","text":format!("http-call-{n}")} ]})
                        }
                        _ => serde_json::json!({}),
                    };
                    let body =
                        serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                            .to_string();
                    let reply=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                    let _ = stream.get_mut().write_all(reply.as_bytes()).await;
                    if first_tool {
                        finished.notify_one();
                    }
                });
            }
        })
    };
    let config = McpServerConfig {
        name: "http-cancel".into(),
        disabled: false,
        source: McpConfigSource::User,
        trust: false,
        auto_approve: vec![],
        allowed_tools: None,
        required_experimental_capabilities: Default::default(),
        config: McpTransportConfig::Http {
            url: format!("http://{address}/mcp"),
            headers: BTreeMap::new(),
            auth: None,
            timeout_ms: Some(5_000),
        },
    };
    let registry = McpRegistry::new(NO_TREE).share();
    registry.add_server(config.clone()).await.unwrap();
    let adapter = Arc::new(
        McpToolAdapter::new(registry.clone(), registry.list_all_tools().await.remove(0)).unwrap(),
    );
    let cancel = CancellationToken::new();
    let task = {
        let tool = adapter.clone();
        let mut context = ctx();
        context.cancel = cancel.clone();
        tokio::spawn(async move { tool.execute("{}", &context).await })
    };
    entered.notified().await;
    let parallel = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.execute("{}", &ctx()).await })
    };
    parallel_entered.notified().await;
    cancel.cancel();
    let result = task.await.unwrap();
    assert!(result.is_error && result.content.contains("unverified"));
    assert!(
        matches!(&registry.server_statuses().await[0].1,ServerStatus::Failed(message) if message.contains("cancelled after dispatch"))
    );
    assert!(adapter.execute("{}", &ctx()).await.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    parallel_release.notify_one();
    let concurrent_result = parallel.await.unwrap();
    assert!(!concurrent_result.is_error && concurrent_result.content.contains("http-call-2"));
    assert!(
        adapter.execute("{}", &ctx()).await.is_error,
        "another completed call cannot clear the cancelled call's uncertainty"
    );
    release.notify_one();
    finished.notified().await;
    assert!(
        adapter.execute("{}", &ctx()).await.is_error,
        "late remote completion does not reopen the old client"
    );
    registry.add_server(config).await.unwrap();
    let result = adapter.execute("{}", &ctx()).await;
    assert!(!result.is_error && result.content.contains("http-call-3"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "only the two accepted calls and explicitly requested next call execute"
    );
    server.abort();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shutdown_awaits_unpublished_candidates_and_fences_late_connections() {
    let _clock = ExternalIoClock::hold();
    for (background, forced_drop) in [(false, false), (true, false), (false, true)] {
        let scratch = tempfile::tempdir().unwrap();
        let began = scratch.path().join("initializing");
        let gate = scratch.path().join("release");
        let mut config = test_server_config_with_env(
            "shutdown",
            BTreeMap::from([
                (
                    "MCP_TEST_INITIALIZE_STARTED".into(),
                    began.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_TEST_INITIALIZE_GATE".into(),
                    gate.to_string_lossy().into_owned(),
                ),
            ]),
        );
        config.source = McpConfigSource::Driver;
        let registry = if background {
            McpRegistry::from_config_background_with_extra(
                scratch.path(),
                scratch.path(),
                None,
                vec![config.clone()],
            )
            .share()
        } else {
            McpRegistry::new(NO_TREE).share()
        };
        tokio::time::resume(); // external startup idle-drain
        let mut adding = if background {
            None
        } else {
            let registry = registry.clone();
            Some(tokio::spawn(
                async move { registry.add_server(config).await },
            ))
        };
        wait_for_tool_counter(began, 1).await;
        tokio::time::pause();
        if forced_drop {
            let adding = adding.take().unwrap();
            adding.abort();
            assert!(adding.await.unwrap_err().is_cancelled());
        }
        if background {
            registry.cancel_pending_work();
            assert!(
                registry
                    .wait_for_initial_connections(Duration::from_secs(1))
                    .await,
                "cancelled startup reports completion before asynchronous resource cleanup"
            );
        }
        let closing = registry.shutdown();
        tokio::pin!(closing);
        assert!(
            futures::poll!(closing.as_mut()).is_pending(),
            "shutdown starts its fence and awaits unpublished candidate cleanup"
        );
        assert!(registry
            .call_tool("shutdown", "echo", serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("shut down"));
        std::fs::write(gate, "release").unwrap();
        closing.await;
        if let Some(adding) = adding {
            assert!(adding.await.unwrap().is_err());
        }
        registry.shutdown().await;
        assert!(registry.listed_tools().is_empty());
        assert!(registry.list_all_tools().await.is_empty());
        assert!(registry
            .add_server(test_server_config("late"))
            .await
            .unwrap_err()
            .to_string()
            .contains("shut down"));
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn aborted_replacement_keeps_old_transport_cleanup_inside_shutdown_barrier() {
    let _clock = ExternalIoClock::hold();
    for reconnect in [false, true] {
        let scratch = tempfile::tempdir().unwrap();
        let eof = scratch.path().join("eof");
        let disposed = scratch.path().join("disposed");
        let gate = scratch.path().join("release");
        let registry = McpRegistry::new(NO_TREE).share();
        let old = test_server_config_with_env(
            "replace",
            BTreeMap::from([
                (
                    "MCP_TEST_EOF_STARTED".into(),
                    eof.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_TEST_EOF_FINISHED".into(),
                    disposed.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_TEST_EOF_GATE".into(),
                    gate.to_string_lossy().into_owned(),
                ),
            ]),
        );
        tokio::time::resume();
        registry.add_server(old).await.unwrap();
        let replacing = {
            let registry = registry.clone();
            tokio::spawn(async move {
                let new = test_server_config("replace");
                if reconnect {
                    registry.reconnect_server(new).await
                } else {
                    registry.add_server(new).await
                }
            })
        };
        wait_for_tool_counter(eof, 1).await;
        tokio::time::pause();
        replacing.abort();
        assert!(replacing.await.unwrap_err().is_cancelled());
        let shutdown = registry.shutdown();
        tokio::pin!(shutdown);
        assert!(
            futures::poll!(shutdown.as_mut()).is_pending(),
            "aborting a replacement does not abandon old transport disposal"
        );
        assert!(!disposed.exists());
        std::fs::write(gate, "release").unwrap();
        assert_eq!(shutdown.await, 0);
        assert!(disposed.exists());
    }
}

fn scoped_config(
    name: &str,
    scratch: &std::path::Path,
    extra: BTreeMap<String, String>,
) -> McpServerConfig {
    let mut env = BTreeMap::from([
        ("MCP_TEST_TURN_SCOPES".into(), "1".into()),
        (
            "MCP_TEST_SCOPE_LOG".into(),
            scratch.join("scope-log").to_string_lossy().into_owned(),
        ),
        (
            "MCP_TEST_TOOL_CALL_COUNTER".into(),
            scratch.join("calls").to_string_lossy().into_owned(),
        ),
    ]);
    env.extend(extra);
    test_server_config_with_env(name, env)
}
fn scope_records(scratch: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(scratch.join("scope-log"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn negotiated_scopes_exclude_other_turns_and_do_not_trust_model_metadata() {
    use atomcode_capabilities::mcp::McpScope;
    let _clock = ExternalIoClock::hold();
    let scratch = tempfile::tempdir().unwrap();
    let registry = McpRegistry::new(NO_TREE).share();
    tokio::time::resume();
    registry
        .add_server(scoped_config("scoped", scratch.path(), BTreeMap::new()))
        .await
        .unwrap();
    registry.list_all_tools().await;
    tokio::time::pause();
    assert!(registry
        .call_tool("scoped", "echo", serde_json::json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("host turn scope"));
    assert!(
        scope_records(scratch.path()).is_empty(),
        "no unscoped RPC was dispatched"
    );
    let a = McpScope::from_identity("lead", 1);
    let b = McpScope::from_identity("member", 1);
    let next = McpScope::from_identity("lead", 2);
    assert_ne!(a, b);
    assert_ne!(a, next);
    assert!(a.owner().len() < 100);
    let call = |scope: McpScope| {
        let registry = registry.clone();
        async move {
            registry
                .call_tool_scoped(
                    "scoped",
                    "echo",
                    serde_json::json!({"_meta":{"atomcode/turn":"forged-model-owner"}}),
                    Some(&scope),
                )
                .await
        }
    };
    assert!(!call(a.clone()).await.unwrap().is_error);
    assert!(call(b.clone())
        .await
        .unwrap_err()
        .to_string()
        .contains("busy"));
    registry.finish_scope(&b).await;
    assert!(
        !call(a.clone()).await.unwrap().is_error,
        "a child's refused scope cannot release its lead"
    );
    assert_eq!(
        scope_records(scratch.path())
            .iter()
            .filter(|v| v["method"] == "atomcode/desktop/acquire")
            .count(),
        1
    );
    registry.finish_scope(&a).await;
    assert!(!call(b.clone()).await.unwrap().is_error);
    let before = scope_records(scratch.path()).len();
    registry.finish_scope(&a).await;
    assert_eq!(
        scope_records(scratch.path()).len(),
        before,
        "late old scope end cannot dispatch a release for the new owner"
    );
    assert!(!call(b.clone()).await.unwrap().is_error);
    registry.finish_scope(&b).await;
    let records = scope_records(scratch.path());
    assert_eq!(records.len(), 8);
    assert!(records[..4].iter().all(|v| v["scope"] == a.owner()));
    assert!(records[4..].iter().all(|v| v["scope"] == b.owner()));
    registry.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn malformed_scope_ack_never_dispatches_a_tool_and_requires_reconnect() {
    let _clock = ExternalIoClock::hold();
    for ack in [
        "{}",
        r#"{"acquired":"true"}"#,
        r#"{"acquired":true,"isError":"false"}"#,
        r#"{"acquired":true,"isError":true}"#,
    ] {
        let scratch = tempfile::tempdir().unwrap();
        let registry = McpRegistry::new(NO_TREE);
        tokio::time::resume();
        registry
            .add_server(scoped_config(
                "bad",
                scratch.path(),
                BTreeMap::from([("MCP_TEST_SCOPE_BAD_ACQUIRE_ACK".into(), ack.into())]),
            ))
            .await
            .unwrap();
        tokio::time::pause();
        let scope = atomcode_capabilities::mcp::McpScope::from_identity("actual-host", 4);
        assert!(registry
            .call_tool_scoped("bad", "echo", serde_json::json!({}), Some(&scope))
            .await
            .unwrap_err()
            .to_string()
            .contains("acknowledgement"));
        assert!(!scratch.path().join("calls").exists());
        assert!(registry
            .call_tool_scoped("bad", "echo", serde_json::json!({}), Some(&scope))
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown"));
        registry.finish_scope(&scope).await;
        assert!(matches!(
            &registry.server_statuses().await[0].1,
            ServerStatus::Failed(_)
        ));
        registry.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancelled_acquire_is_not_replayed_and_scope_end_retires_its_connection() {
    let _clock = ExternalIoClock::hold();
    let scratch = tempfile::tempdir().unwrap();
    let gate = scratch.path().join("release-acquire");
    let config = scoped_config(
        "scoped",
        scratch.path(),
        BTreeMap::from([(
            "MCP_TEST_SCOPE_ACQUIRE_GATE".into(),
            gate.to_string_lossy().into_owned(),
        )]),
    );
    let registry = McpRegistry::new(NO_TREE).share();
    tokio::time::resume();
    registry.add_server(config.clone()).await.unwrap();
    let info = registry.list_all_tools().await.remove(0);
    tokio::time::pause();
    let scope = atomcode_capabilities::mcp::McpScope::from_identity("cancelled", 1);
    let provided = scope.clone();
    let adapter = Arc::new(
        McpToolAdapter::new(registry.clone(), info)
            .unwrap()
            .with_scope_provider(Arc::new(move || Some(provided.clone()))),
    );
    let ctx = ToolContext {
        working_dir: scratch.path().to_path_buf(),
        cancel: CancellationToken::new(),
        progress: ProgressSink::noop(),
        requester: None,
    };
    let cancel = ctx.cancel.clone();
    let running = tokio::spawn(async move { adapter.execute("{}", &ctx).await });
    wait_for_tool_counter(scratch.path().join("scope-log"), 1).await;
    cancel.cancel();
    assert!(running.await.unwrap().is_error);
    assert!(matches!(
        &registry.server_statuses().await[0].1,
        ServerStatus::Failed(_)
    ));
    std::fs::write(&gate, "late acknowledgement").unwrap();
    registry.finish_scope(&scope).await;
    assert!(!scratch.path().join("calls").exists());
    assert_eq!(scope_records(scratch.path()).len(), 1, "the interrupted acquire was not retried and no release/tool was sent into an uncertain stream");
    tokio::time::resume();
    registry.add_server(config).await.unwrap();
    tokio::time::pause();
    let fresh = atomcode_capabilities::mcp::McpScope::from_identity("cancelled", 2);
    assert!(
        !registry
            .call_tool_scoped("scoped", "echo", serde_json::json!({}), Some(&fresh))
            .await
            .unwrap()
            .is_error
    );
    registry.finish_scope(&fresh).await;
    registry.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn queued_scope_cancellation_and_cleanup_do_not_disrupt_the_executing_owner() {
    use atomcode_capabilities::mcp::McpScope;
    let _clock = ExternalIoClock::hold();
    let scratch = tempfile::tempdir().unwrap();
    let gate = scratch.path().join("tool-gate");
    let registry = McpRegistry::new(NO_TREE).share();
    tokio::time::resume();
    registry
        .add_server(scoped_config(
            "scoped",
            scratch.path(),
            BTreeMap::from([(
                "MCP_TEST_TOOL_RESPONSE_GATE".into(),
                gate.to_string_lossy().into_owned(),
            )]),
        ))
        .await
        .unwrap();
    tokio::time::pause();
    let a = McpScope::from_identity("lead", 1);
    let b = McpScope::from_identity("child", 1);
    let first = {
        let reg = registry.clone();
        let scope = a.clone();
        tokio::spawn(async move {
            reg.call_tool_scoped(
                "scoped",
                "echo",
                serde_json::json!({"message":"lead"}),
                Some(&scope),
            )
            .await
        })
    };
    wait_for_tool_counter(scratch.path().join("calls"), 1).await;
    let queued = {
        let reg = registry.clone();
        let scope = b.clone();
        tokio::spawn(async move {
            reg.call_tool_scoped("scoped", "echo", serde_json::json!({}), Some(&scope))
                .await
        })
    };
    tokio::task::yield_now().await;
    queued.abort();
    assert!(queued.await.unwrap_err().is_cancelled());
    let end = registry.finish_scope(&b);
    tokio::pin!(end);
    assert!(
        futures::poll!(end.as_mut()).is_ready(),
        "ending a queued child must not wait behind its lead's tool"
    );
    assert_eq!(scope_records(scratch.path()).len(), 2);
    assert!(matches!(
        registry.server_statuses().await[0].1,
        ServerStatus::Connected
    ));
    std::fs::write(gate, "finish lead").unwrap();
    assert!(result_text(first.await.unwrap().unwrap()).contains("lead"));
    registry.finish_scope(&a).await;
    assert_eq!(scope_records(scratch.path()).len(), 3);
    assert_eq!(scope_records(scratch.path())[2]["scope"], a.owner());
    registry.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unsupported_desktop_scope_version_fails_before_tools_are_published() {
    let _clock = ExternalIoClock::hold();
    let scratch = tempfile::tempdir().unwrap();
    let registry = McpRegistry::new(NO_TREE);
    tokio::time::resume();
    let result = registry
        .add_server(scoped_config(
            "future",
            scratch.path(),
            BTreeMap::from([("MCP_TEST_SCOPE_VERSION".into(), "2".into())]),
        ))
        .await;
    tokio::time::pause();
    assert!(result.is_err());
    assert!(registry.list_all_tools().await.is_empty());
    assert!(scope_records(scratch.path()).is_empty());
    registry.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timed_out_scope_release_is_not_replayed_or_reused() {
    use atomcode_capabilities::mcp::McpScope;
    let _clock = ExternalIoClock::hold();
    let scratch = tempfile::tempdir().unwrap();
    let gate = scratch.path().join("release-gate");
    let registry = McpRegistry::new(NO_TREE).share();
    tokio::time::resume();
    registry
        .add_server(scoped_config(
            "scoped",
            scratch.path(),
            BTreeMap::from([(
                "MCP_TEST_SCOPE_RELEASE_GATE".into(),
                gate.to_string_lossy().into_owned(),
            )]),
        ))
        .await
        .unwrap();
    tokio::time::pause();
    let a = McpScope::from_identity("lead", 1);
    registry
        .call_tool_scoped("scoped", "echo", serde_json::json!({}), Some(&a))
        .await
        .unwrap();
    let ending = {
        let reg = registry.clone();
        let scope = a.clone();
        tokio::spawn(async move { reg.finish_scope(&scope).await })
    };
    wait_for_tool_counter(scratch.path().join("scope-log"), 3).await;
    tokio::time::advance(Duration::from_secs(6)).await;
    // Observe retirement before allowing the external late response; the
    // paused timer has millisecond granularity at its exact deadline.
    for _ in 0..100 {
        if matches!(
            registry.server_statuses().await[0].1,
            ServerStatus::Disconnected | ServerStatus::Failed(_)
        ) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(!matches!(
        registry.server_statuses().await[0].1,
        ServerStatus::Connected
    ));
    std::fs::write(gate, "late release acknowledgement").unwrap();
    ending.await.unwrap();
    assert!(matches!(
        registry.server_statuses().await[0].1,
        ServerStatus::Failed(_)
    ));
    let next = McpScope::from_identity("lead", 2);
    assert!(registry
        .call_tool_scoped("scoped", "echo", serde_json::json!({}), Some(&next))
        .await
        .is_err());
    registry.finish_scope(&a).await;
    let records = scope_records(scratch.path());
    assert_eq!(
        records.len(),
        3,
        "no release replay and no later acquire/tool on an uncertain stream"
    );
    assert_eq!(records[2]["method"], "atomcode/desktop/release");
    assert_eq!(spawn_count(&scratch.path().join("calls")), 1);
    registry.shutdown().await;
}

/// Explicit opt-in to the patched native executor, direct protocol only. No
/// desktop enumeration, capture, input, installed App socket or TCC changes.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires ATOMCODE_LEASE_TEST_EXECUTABLE; no GUI actions"]
async fn native_desktop_lease_hands_over_between_production_registries() {
    use atomcode_capabilities::mcp::McpScope;
    let executable =
        std::fs::canonicalize(std::env::var("ATOMCODE_LEASE_TEST_EXECUTABLE").unwrap()).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let make_config = |profile: &str| McpServerConfig {
        name: "desktop".into(),
        source: McpConfigSource::Driver,
        disabled: false,
        trust: false,
        auto_approve: vec![],
        allowed_tools: None,
        required_experimental_capabilities: Default::default(),
        config: McpTransportConfig::Stdio {
            command: executable.to_string_lossy().into_owned(),
            args: vec!["mcp".into()],
            env: BTreeMap::from([
                (
                    "OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY".into(),
                    "1".into(),
                ),
                (
                    "OPEN_COMPUTER_USE_TURN_LEASE_DIRECTORY".into(),
                    scratch.path().join("leases").to_string_lossy().into_owned(),
                ),
                (
                    "ATOMCODE_HOME".into(),
                    scratch.path().join(profile).to_string_lossy().into_owned(),
                ),
            ]),
            timeout_ms: Some(10_000),
        },
    };
    let a = McpRegistry::new(scratch.path().join("a"));
    let b = McpRegistry::new(scratch.path().join("b"));
    a.add_server(make_config("a")).await.unwrap();
    b.add_server(make_config("b")).await.unwrap();
    let first = McpScope::from_identity("lead-a", 1);
    let second = McpScope::from_identity("lead-b", 1);
    let unknown = "__nonexistent_test_tool__";
    let result = a
        .call_tool_scoped("desktop", unknown, serde_json::json!({}), Some(&first))
        .await
        .unwrap();
    assert!(result.is_error && result.content.iter().any(|c| matches!(c, atomcode_capabilities::mcp::ContentBlock::Text { text } if text.contains("unsupportedTool"))), "unknown tool is rejected only after scoped acquisition; no GUI action");
    let busy = b
        .call_tool_scoped("desktop", unknown, serde_json::json!({}), Some(&second))
        .await
        .unwrap_err();
    assert!(format!("{busy:#}").contains("busy"));
    b.finish_scope(&second).await;
    assert!(
        b.call_tool_scoped("desktop", unknown, serde_json::json!({}), Some(&second))
            .await
            .is_err(),
        "ending a refused owner cannot unlock a neighbor"
    );
    a.finish_scope(&first).await;
    assert!(matches!(
        a.server_statuses().await[0].1,
        ServerStatus::Connected
    ));
    let result = b
        .call_tool_scoped("desktop", unknown, serde_json::json!({}), Some(&second))
        .await
        .unwrap();
    assert!(result.is_error && result.content.iter().any(|c| matches!(c, atomcode_capabilities::mcp::ContentBlock::Text { text } if text.contains("unsupportedTool"))));
    a.finish_scope(&first).await;
    b.finish_scope(&second).await;
    assert!(matches!(
        b.server_statuses().await[0].1,
        ServerStatus::Connected
    ));
    assert_eq!(a.shutdown().await, 0);
    assert_eq!(b.shutdown().await, 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn scoped_connection_loss_requires_explicit_reconnect_without_replaying_a_tool() {
    use atomcode_capabilities::mcp::McpScope;
    let _clock = ExternalIoClock::hold();
    let scratch = tempfile::tempdir().unwrap();
    let registry = McpRegistry::new(NO_TREE);
    let spawns = scratch.path().join("spawns");
    tokio::time::resume();
    registry
        .add_server(scoped_config(
            "scoped",
            scratch.path(),
            BTreeMap::from([
                ("MCP_TEST_EXIT_ON_EVERY_TOOL_CALL".into(), "1".into()),
                (
                    "MCP_TEST_SPAWN_COUNTER".into(),
                    spawns.to_string_lossy().into_owned(),
                ),
            ]),
        ))
        .await
        .unwrap();
    tokio::time::pause();
    let a = McpScope::from_identity("lead", 1);
    let error = registry
        .call_tool_scoped("scoped", "echo", serde_json::json!({}), Some(&a))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("not replayed"));
    assert!(matches!(
        registry.server_statuses().await[0].1,
        ServerStatus::Failed(_)
    ));
    let b = McpScope::from_identity("lead", 2);
    assert!(registry
        .call_tool_scoped("scoped", "echo", serde_json::json!({}), Some(&b))
        .await
        .is_err());
    registry.finish_scope(&a).await;
    assert_eq!(
        spawn_count(&spawns),
        1,
        "no replacement process is silently created for a scoped operation"
    );
    let records = scope_records(scratch.path());
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["method"], "atomcode/desktop/acquire");
    assert_eq!(records[1]["method"], "tools/call");
    registry.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn negotiated_scope_protocol_cannot_disappear_during_transport_recovery() {
    let _clock = ExternalIoClock::hold();
    let scratch = tempfile::tempdir().unwrap();
    let registry = McpRegistry::new(NO_TREE);
    let marker = scratch.path().join("disable-scopes");
    let spawns = scratch.path().join("spawns");
    tokio::time::resume();
    registry
        .add_server(scoped_config(
            "scoped",
            scratch.path(),
            BTreeMap::from([
                ("MCP_TEST_EXIT_ON_EVERY_TOOLS_LIST".into(), "1".into()),
                (
                    "MCP_TEST_SCOPE_DISABLE_MARKER".into(),
                    marker.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_TEST_SPAWN_COUNTER".into(),
                    spawns.to_string_lossy().into_owned(),
                ),
            ]),
        ))
        .await
        .unwrap();
    std::fs::write(marker, "replacement no longer supports leases").unwrap();
    // The external replacement handshake has the normal startup idle-drain.
    assert!(registry.list_all_tools().await.is_empty());
    tokio::time::pause();
    assert_eq!(spawn_count(&spawns), 2);
    assert!(matches!(
        registry.server_statuses().await[0].1,
        ServerStatus::Failed(_)
    ));
    assert!(
        scope_records(scratch.path()).is_empty(),
        "no acquire or action was dispatched through a downgraded connection"
    );
    registry.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn ending_a_scope_waits_for_its_accepted_acquire_before_releasing_it() {
    use atomcode_capabilities::mcp::McpScope;
    let _clock = ExternalIoClock::hold();
    let scratch = tempfile::tempdir().unwrap();
    let gate = scratch.path().join("acquire-gate");
    let registry = McpRegistry::new(NO_TREE).share();
    tokio::time::resume();
    registry
        .add_server(scoped_config(
            "scoped",
            scratch.path(),
            BTreeMap::from([(
                "MCP_TEST_SCOPE_ACQUIRE_GATE".into(),
                gate.to_string_lossy().into_owned(),
            )]),
        ))
        .await
        .unwrap();
    tokio::time::pause();
    let scope = McpScope::from_identity("lead", 1);
    let calling = {
        let reg = registry.clone();
        let scope = scope.clone();
        tokio::spawn(async move {
            reg.call_tool_scoped("scoped", "echo", serde_json::json!({}), Some(&scope))
                .await
        })
    };
    wait_for_tool_counter(scratch.path().join("scope-log"), 1).await;
    let ending = registry.finish_scope(&scope);
    tokio::pin!(ending);
    assert!(
        futures::poll!(ending.as_mut()).is_pending(),
        "accepted acquire cannot be forgotten before its acknowledgement"
    );
    std::fs::write(gate, "acknowledge acquire").unwrap();
    assert!(!calling.await.unwrap().unwrap().is_error);
    ending.await;
    let records = scope_records(scratch.path());
    assert_eq!(records.len(), 3);
    assert_eq!(records[2]["method"], "atomcode/desktop/release");
    assert!(matches!(
        registry.server_statuses().await[0].1,
        ServerStatus::Connected
    ));
    registry.shutdown().await;
}

fn require_test_capability(config: &mut McpServerConfig) {
    config.required_experimental_capabilities = BTreeMap::from([("test/strict".into(), 1)]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn required_capabilities_refuse_publication_on_initial_mismatch() {
    // The transport's startup noise drain has a real idle deadline, including
    // reconnect startup. These real-process checks make no virtual-time claims.
    tokio::time::resume();
    for capabilities in [
        "{}",
        r#"{"test/strict":{"version":true}}"#,
        r#"{"test/strict":{"version":"1"}}"#,
        r#"{"test/strict":{"version":2}}"#,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let calls = temp.path().join("calls");
        let mut config = test_server_config_with_env(
            "required",
            BTreeMap::from([
                (
                    "MCP_TEST_EXPERIMENTAL_CAPABILITIES".into(),
                    capabilities.into(),
                ),
                (
                    "MCP_TEST_TOOL_CALL_COUNTER".into(),
                    calls.display().to_string(),
                ),
            ]),
        );
        require_test_capability(&mut config);
        let reg = McpRegistry::new(NO_TREE);
        assert!(reg
            .add_server(config)
            .await
            .unwrap_err()
            .to_string()
            .contains("required experimental capability"));
        assert!(reg.list_all_tools().await.is_empty());
        assert!(reg
            .call_tool("required", "echo", serde_json::json!({}))
            .await
            .is_err());
        assert_eq!(spawn_count(&calls), 0);
        reg.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn required_capabilities_survive_automatic_reconnect_and_reject_stale_adapters() {
    // The transport's startup noise drain has a real idle deadline, including
    // reconnect startup. These real-process checks make no virtual-time claims.
    tokio::time::resume();
    let temp = tempfile::tempdir().unwrap();
    let dropped = temp.path().join("drop-capabilities");
    let disconnect = temp.path().join("disconnect-on-list");
    let calls = temp.path().join("calls");
    let spawns = temp.path().join("spawns");
    let mut config = test_server_config_with_env(
        "required",
        BTreeMap::from([
            (
                "MCP_TEST_EXPERIMENTAL_CAPABILITIES".into(),
                r#"{"test/strict":{"version":1}}"#.into(),
            ),
            (
                "MCP_TEST_CAPABILITIES_DISABLE_MARKER".into(),
                dropped.display().to_string(),
            ),
            (
                "MCP_TEST_EXIT_TOOLS_LIST_MARKER".into(),
                disconnect.display().to_string(),
            ),
            (
                "MCP_TEST_TOOL_CALL_COUNTER".into(),
                calls.display().to_string(),
            ),
            (
                "MCP_TEST_SPAWN_COUNTER".into(),
                spawns.display().to_string(),
            ),
        ]),
    );
    require_test_capability(&mut config);
    let reg = Arc::new(McpRegistry::new(NO_TREE));
    reg.add_server(config).await.unwrap();
    let info = reg.list_all_tools().await.remove(0);
    let stale = McpToolAdapter::new(reg.clone(), info).unwrap();
    std::fs::write(&dropped, "").unwrap();
    std::fs::write(&disconnect, "").unwrap();
    assert!(reg.list_all_tools().await.is_empty());
    assert!(
        stale
            .execute(r#"{"message":"must not dispatch"}"#, &ctx())
            .await
            .is_error
    );
    assert_eq!(spawn_count(&spawns), 2);
    assert_eq!(spawn_count(&calls), 0);
    reg.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn required_capabilities_survive_background_unknown_outcome_recovery() {
    // The transport's startup noise drain has a real idle deadline, including
    // reconnect startup. These real-process checks make no virtual-time claims.
    tokio::time::resume();
    let temp = tempfile::tempdir().unwrap();
    let dropped = temp.path().join("drop-capabilities");
    let calls = temp.path().join("calls");
    let disposed = temp.path().join("disposed");
    let mut config = test_server_config_with_env(
        "required",
        BTreeMap::from([
            (
                "MCP_TEST_EXPERIMENTAL_CAPABILITIES".into(),
                r#"{"test/strict":{"version":1}}"#.into(),
            ),
            (
                "MCP_TEST_CAPABILITIES_DISABLE_MARKER".into(),
                dropped.display().to_string(),
            ),
            ("MCP_TEST_EXIT_ON_EVERY_TOOL_CALL".into(), "1".into()),
            (
                "MCP_TEST_TOOL_CALL_COUNTER".into(),
                calls.display().to_string(),
            ),
            (
                "MCP_TEST_EOF_FINISHED".into(),
                disposed.display().to_string(),
            ),
        ]),
    );
    require_test_capability(&mut config);
    let reg = McpRegistry::new(NO_TREE);
    reg.add_server(config).await.unwrap();
    std::fs::write(&dropped, "").unwrap();
    let error = reg
        .call_tool("required", "echo", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("unknown"));
    wait_for_tool_counter(disposed, 1).await;
    assert!(reg
        .call_tool("required", "echo", serde_json::json!({}))
        .await
        .is_err());
    assert_eq!(
        spawn_count(&calls),
        1,
        "uncertain mutation is never replayed on a downgraded connection"
    );
    reg.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn required_capabilities_allow_reconnect_when_the_contract_is_preserved() {
    tokio::time::resume(); // Startup noise drain uses a real idle deadline.
    let temp = tempfile::tempdir().unwrap();
    let spawns = temp.path().join("spawns");
    let calls = temp.path().join("calls");
    let once = temp.path().join("exit-once");
    let mut config = test_server_config_with_env(
        "required",
        BTreeMap::from([
            (
                "MCP_TEST_EXPERIMENTAL_CAPABILITIES".into(),
                r#"{"test/strict":{"version":1}}"#.into(),
            ),
            (
                "MCP_TEST_EXIT_ONCE_TOOLS_LIST_MARKER".into(),
                once.display().to_string(),
            ),
            (
                "MCP_TEST_TOOL_CALL_COUNTER".into(),
                calls.display().to_string(),
            ),
            (
                "MCP_TEST_SPAWN_COUNTER".into(),
                spawns.display().to_string(),
            ),
        ]),
    );
    require_test_capability(&mut config);
    let reg = McpRegistry::new(NO_TREE);
    reg.add_server(config).await.unwrap();
    assert_eq!(reg.list_all_tools().await.len(), 1);
    let result = reg
        .call_tool(
            "required",
            "echo",
            serde_json::json!({"message":"preserved"}),
        )
        .await
        .unwrap();
    assert!(result_text(result).contains("preserved"));
    assert_eq!(spawn_count(&spawns), 2);
    assert_eq!(spawn_count(&calls), 1);
    reg.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn required_capabilities_http_handshake_rejects_missing_or_mistyped_contracts() {
    use atomcode_capabilities::mcp::client::McpClient;
    use atomcode_capabilities::mcp::transport_http::HttpClient;
    use atomcode_capabilities::mcp::McpTokenStore;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let _clock = ExternalIoClock::hold();
    for declaration in [
        serde_json::json!(null),
        serde_json::json!({"version":true}),
        serde_json::json!({"version":"1"}),
        serde_json::json!({"version":2}),
    ] {
        let server = MockServer::start().await;
        let caps = serde_json::json!({"experimental":{"test/strict":declaration}});
        Mock::given(method("POST")).and(body_partial_json(serde_json::json!({"method":"initialize"})))
            .respond_with(move |request: &wiremock::Request| {
                let body:serde_json::Value=request.body_json().unwrap();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"jsonrpc":"2.0","id":body["id"],
                    "result":{"protocolVersion":"2025-11-05","capabilities":caps,"serverInfo":{"name":"required","version":"1"}}}))
            }).expect(1).mount(&server).await;
        let mut client = HttpClient::new_with_required_experimental_capabilities(
            "required".into(),
            server.uri(),
            BTreeMap::new(),
            None,
            Some(2000),
            McpTokenStore::new("/nonexistent/mcp_auth.toml".into()),
            BTreeMap::from([("test/strict".into(), 1)]),
        );
        assert!(client
            .list_tools()
            .await
            .unwrap_err()
            .to_string()
            .contains("not been validated"));
        assert!(client
            .initialize()
            .await
            .unwrap_err()
            .to_string()
            .contains("required experimental capability"));
        assert!(client
            .call_tool("never-dispatched", serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("not been validated"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        client.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn required_capabilities_http_session_rotation_fails_closed_without_tool_dispatch() {
    use atomcode_capabilities::mcp::client::McpClient;
    use atomcode_capabilities::mcp::transport_http::HttpClient;
    use atomcode_capabilities::mcp::McpTokenStore;
    use wiremock::matchers::{body_partial_json, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let _clock = ExternalIoClock::hold();
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(body_partial_json(serde_json::json!({"method":"initialize"})))
        .respond_with(|request: &wiremock::Request| {
            let body:serde_json::Value=request.body_json().unwrap();
            ResponseTemplate::new(200).insert_header("mcp-session-id","initial-private-session")
                .set_body_json(serde_json::json!({"jsonrpc":"2.0","id":body["id"],"result":{
                    "protocolVersion":"2025-11-05","capabilities":{"experimental":{"test/strict":{"version":1}}},
                    "serverInfo":{"name":"required","version":"1"}}}))
        }).expect(1).mount(&server).await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"method":"notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let listings = std::sync::atomic::AtomicUsize::new(0);
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"method":"tools/list"}),
        ))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            let sid = if listings.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                "initial-private-session"
            } else {
                "rotated-private-session"
            };
            ResponseTemplate::new(200)
                .insert_header("mcp-session-id", sid)
                .set_body_json(
                    serde_json::json!({"jsonrpc":"2.0","id":body["id"],"result":{"tools":[]}}),
                )
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(header("mcp-session-id", "initial-private-session"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = HttpClient::new_with_required_experimental_capabilities(
        "required".into(),
        server.uri(),
        BTreeMap::new(),
        None,
        Some(2000),
        McpTokenStore::new("/nonexistent/mcp_auth.toml".into()),
        BTreeMap::from([("test/strict".into(), 1)]),
    );
    client.initialize().await.unwrap();
    assert!(client.list_tools().await.unwrap().tools.is_empty());
    let error = client.list_tools().await.unwrap_err().to_string();
    assert!(error.contains("session identity changed"));
    assert!(
        !error.contains("initial-private-session") && !error.contains("rotated-private-session")
    );
    assert!(client
        .call_tool("never-dispatched", serde_json::json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("not been validated"));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    client.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn required_capabilities_invalid_driver_requirements_do_not_spawn_a_backend() {
    let temp = tempfile::tempdir().unwrap();
    let spawns = temp.path().join("spawns");
    let mut config = test_server_config_with_env(
        "invalid-required",
        BTreeMap::from([(
            "MCP_TEST_SPAWN_COUNTER".into(),
            spawns.display().to_string(),
        )]),
    );
    config.required_experimental_capabilities = BTreeMap::from([("test/strict".into(), 0)]);
    let reg = McpRegistry::new(NO_TREE);
    assert!(reg
        .add_server(config)
        .await
        .unwrap_err()
        .to_string()
        .contains("invalid required MCP"));
    reg.shutdown().await;
    assert_eq!(spawn_count(&spawns), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "requires an application-scope-capable ATOMCODE_LEASE_TEST_EXECUTABLE; no GUI actions"]
async fn native_required_capabilities_gate_production_registry() {
    tokio::time::resume(); // Real native process startup uses an idle noise-drain deadline.
    let executable =
        std::fs::canonicalize(std::env::var("ATOMCODE_LEASE_TEST_EXECUTABLE").unwrap()).unwrap();
    let make_config = |version| McpServerConfig {
        name: "native-required".into(),
        source: McpConfigSource::Driver,
        disabled: false,
        trust: false,
        auto_approve: vec![],
        allowed_tools: None,
        required_experimental_capabilities: BTreeMap::from([(
            "atomcode/application-scope".into(),
            version,
        )]),
        config: McpTransportConfig::Stdio {
            command: executable.display().to_string(),
            args: vec!["mcp".into()],
            env: BTreeMap::from([(
                "OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY".into(),
                "1".into(),
            )]),
            timeout_ms: Some(10_000),
        },
    };
    let reg = McpRegistry::new(NO_TREE);
    reg.add_server(make_config(1)).await.unwrap();
    assert_eq!(
        reg.list_all_tools().await.len(),
        15,
        "metadata only; no GUI tool requested"
    );
    assert!(reg
        .reconnect_server(make_config(2))
        .await
        .unwrap_err()
        .to_string()
        .contains("required experimental capability"));
    assert!(reg.list_all_tools().await.is_empty());
    assert!(reg
        .call_tool("native-required", "capture_display", serde_json::json!({}))
        .await
        .is_err());
    reg.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn required_capabilities_apply_to_background_connections_loaded_from_user_config() {
    tokio::time::resume(); // Real external startup idle-drain, not a timing assertion.
    for advertised in [false, true] {
        let scratch = tempfile::tempdir().unwrap();
        let capabilities = if advertised {
            serde_json::json!({"test/strict":{"version":1}})
        } else {
            serde_json::json!({})
        };
        let source = serde_json::json!({"mcpServers":{"required":{
            "command":env!("CARGO_BIN_EXE_mcp-test-server"), "timeout_ms":5000,
            "env":{"MCP_TEST_EXPERIMENTAL_CAPABILITIES":capabilities.to_string()},
            "requiredExperimentalCapabilities":{"test/strict":1}
        }}});
        std::fs::write(scratch.path().join("mcp.json"), source.to_string()).unwrap();
        let reg = McpRegistry::from_config_background_with_extra(
            scratch.path(),
            scratch.path(),
            None,
            Vec::new(),
        );
        let completed = reg.wait_for_initial_connections(CONNECT_TIMEOUT).await;
        assert!(
            completed,
            "advertised={advertised}, statuses={:?}",
            reg.server_statuses().await
        );
        assert_eq!(reg.list_all_tools().await.len(), usize::from(advertised));
        assert_eq!(
            reg.connected_server_names().await.len(),
            usize::from(advertised)
        );
        if !advertised {
            assert!(reg.server_statuses().await.iter().any(|(_, status)| matches!(status,
                ServerStatus::Failed(message) if message.contains("required experimental capability"))),
                "a generic timeout must not count as capability validation");
        }
        reg.shutdown().await;
    }
}
